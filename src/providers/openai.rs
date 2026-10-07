//! OpenAI via the Responses API (`POST /v1/responses`) with an `input_file`
//! PDF part (or `input_text` for converted uploads) and a strict `json_schema`
//! text format.

use async_trait::async_trait;
use serde_json::{Value, json};

use super::{
    Document, ExtractRequest, Provider, ProviderError, api_key, parse_json_output, read_json,
};
use crate::config::OpenAiConfig;

const NAME: &str = "openai";

pub struct OpenAi {
    http: reqwest::Client,
    cfg: OpenAiConfig,
}

impl OpenAi {
    pub fn new(http: reqwest::Client, cfg: OpenAiConfig) -> Self {
        Self { http, cfg }
    }
}

#[async_trait]
impl Provider for OpenAi {
    fn name(&self) -> &'static str {
        NAME
    }

    async fn extract(&self, req: &ExtractRequest<'_>) -> Result<Value, ProviderError> {
        let key = api_key(&self.cfg.api_key_env)?;

        let file_part = match req.document {
            Document::Pdf { base64 } => json!({
                "type": "input_file",
                "filename": req.filename,
                "file_data": format!("data:application/pdf;base64,{base64}")
            }),
            Document::Text { text } => json!({
                "type": "input_text",
                "text": format!("Document \"{}\":\n\n{text}", req.filename)
            }),
        };
        let body = json!({
            "model": req.model,
            "max_output_tokens": req.max_output_tokens,
            "input": [{
                "role": "user",
                "content": [
                    file_part,
                    { "type": "input_text", "text": req.prompt }
                ]
            }],
            "text": {
                "format": {
                    "type": "json_schema",
                    "name": "extraction",
                    "strict": true,
                    "schema": req.schema
                }
            }
        });

        let resp = self
            .http
            .post(format!(
                "{}/v1/responses",
                self.cfg.base_url.trim_end_matches('/')
            ))
            .bearer_auth(key)
            .json(&body)
            .send()
            .await
            .map_err(|source| ProviderError::Http {
                provider: NAME,
                source,
            })?;
        let v = read_json(NAME, resp).await?;

        if v["status"] == "incomplete" {
            let reason = v["incomplete_details"]["reason"].as_str().unwrap_or("");
            if reason == "max_output_tokens" {
                return Err(ProviderError::Truncated { provider: NAME });
            }
            return Err(ProviderError::BadOutput {
                provider: NAME,
                detail: format!("response incomplete: {reason}"),
            });
        }

        let outputs = v["output"].as_array().cloned().unwrap_or_default();
        let message = outputs.iter().find(|o| o["type"] == "message");
        let Some(message) = message else {
            return Err(ProviderError::BadOutput {
                provider: NAME,
                detail: "no message in output".into(),
            });
        };
        let parts = message["content"].as_array().cloned().unwrap_or_default();
        if let Some(r) = parts.iter().find(|p| p["type"] == "refusal") {
            return Err(ProviderError::Refused {
                provider: NAME,
                reason: r["refusal"].as_str().unwrap_or("model refused").to_string(),
            });
        }
        let text = parts
            .iter()
            .find(|p| p["type"] == "output_text")
            .and_then(|p| p["text"].as_str())
            .ok_or_else(|| ProviderError::BadOutput {
                provider: NAME,
                detail: "no output_text part".into(),
            })?;
        parse_json_output(NAME, text)
    }
}
