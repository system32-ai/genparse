//! Gemini via `generateContent` with an inline PDF part (or a text part for
//! converted uploads) and `response_json_schema` structured output.

use async_trait::async_trait;
use serde_json::{Value, json};

use super::{
    Document, ExtractRequest, Provider, ProviderError, api_key, parse_json_output, read_json,
};
use crate::config::GeminiConfig;

const NAME: &str = "gemini";

pub struct Gemini {
    http: reqwest::Client,
    cfg: GeminiConfig,
}

impl Gemini {
    pub fn new(http: reqwest::Client, cfg: GeminiConfig) -> Self {
        Self { http, cfg }
    }
}

#[async_trait]
impl Provider for Gemini {
    fn name(&self) -> &'static str {
        NAME
    }

    async fn extract(&self, req: &ExtractRequest<'_>) -> Result<Value, ProviderError> {
        let key = api_key(&self.cfg.api_key_env)?;

        let file_part = match req.document {
            Document::Pdf { base64 } => {
                json!({ "inline_data": { "mime_type": "application/pdf", "data": base64 } })
            }
            Document::Text { text } => {
                json!({ "text": format!("Document \"{}\":\n\n{text}", req.filename) })
            }
        };
        let body = json!({
            "contents": [{
                "role": "user",
                "parts": [
                    file_part,
                    { "text": req.prompt }
                ]
            }],
            "generation_config": {
                "response_mime_type": "application/json",
                "response_json_schema": req.schema,
                "max_output_tokens": req.max_output_tokens
            }
        });

        let url = format!(
            "{}/v1beta/models/{}:generateContent",
            self.cfg.base_url.trim_end_matches('/'),
            req.model
        );
        let resp = self
            .http
            .post(url)
            .header("x-goog-api-key", key)
            .json(&body)
            .send()
            .await
            .map_err(|source| ProviderError::Http {
                provider: NAME,
                source,
            })?;
        let v = read_json(NAME, resp).await?;

        if let Some(block) = v["promptFeedback"]["blockReason"].as_str() {
            return Err(ProviderError::Refused {
                provider: NAME,
                reason: format!("prompt blocked: {block}"),
            });
        }
        let candidate = v["candidates"]
            .as_array()
            .and_then(|c| c.first())
            .ok_or_else(|| ProviderError::BadOutput {
                provider: NAME,
                detail: "no candidates".into(),
            })?;
        match candidate["finishReason"].as_str() {
            Some("MAX_TOKENS") => return Err(ProviderError::Truncated { provider: NAME }),
            Some(r @ ("SAFETY" | "RECITATION" | "PROHIBITED_CONTENT" | "BLOCKLIST" | "SPII")) => {
                return Err(ProviderError::Refused {
                    provider: NAME,
                    reason: r.to_string(),
                });
            }
            _ => {}
        }
        let text: String = candidate["content"]["parts"]
            .as_array()
            .map(|parts| {
                parts
                    .iter()
                    .filter(|p| p["thought"] != true)
                    .filter_map(|p| p["text"].as_str())
                    .collect::<Vec<_>>()
                    .concat()
            })
            .unwrap_or_default();
        if text.is_empty() {
            return Err(ProviderError::BadOutput {
                provider: NAME,
                detail: "no text parts in candidate".into(),
            });
        }
        parse_json_output(NAME, &text)
    }
}
