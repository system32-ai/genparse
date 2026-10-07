//! Claude via the Messages API (`POST /v1/messages`) with a `document` block
//! (PDF or plain text) and `output_config.format` structured output.

use async_trait::async_trait;
use serde_json::{Value, json};

use super::{
    Document, ExtractRequest, Provider, ProviderError, api_key, parse_json_output, read_json,
};
use crate::config::AnthropicConfig;

const NAME: &str = "anthropic";
const API_VERSION: &str = "2023-06-01";
const FALLBACK_BETA: &str = "server-side-fallback-2026-07-01";

pub struct Anthropic {
    http: reqwest::Client,
    cfg: AnthropicConfig,
}

impl Anthropic {
    pub fn new(http: reqwest::Client, cfg: AnthropicConfig) -> Self {
        Self { http, cfg }
    }

    fn supports_fallbacks(model: &str) -> bool {
        // Haiku has no server-side fallback; the other current models do.
        !model.starts_with("claude-haiku")
    }
}

#[async_trait]
impl Provider for Anthropic {
    fn name(&self) -> &'static str {
        NAME
    }

    async fn extract(&self, req: &ExtractRequest<'_>) -> Result<Value, ProviderError> {
        let key = api_key(&self.cfg.api_key_env)?;
        let use_fallbacks = self.cfg.fallbacks && Self::supports_fallbacks(req.model);

        let source = match req.document {
            Document::Pdf { base64 } => json!({
                "type": "base64",
                "media_type": "application/pdf",
                "data": base64
            }),
            Document::Text { text } => json!({
                "type": "text",
                "media_type": "text/plain",
                "data": text
            }),
        };
        let mut body = json!({
            "model": req.model,
            "max_tokens": req.max_output_tokens,
            "messages": [{
                "role": "user",
                "content": [
                    { "type": "document", "title": req.filename, "source": source },
                    { "type": "text", "text": req.prompt }
                ]
            }],
            "output_config": {
                "format": { "type": "json_schema", "schema": req.schema }
            }
        });
        if use_fallbacks {
            body["fallbacks"] = json!("default");
        }

        let mut request = self
            .http
            .post(format!(
                "{}/v1/messages",
                self.cfg.base_url.trim_end_matches('/')
            ))
            .header("x-api-key", key)
            .header("anthropic-version", API_VERSION)
            .json(&body);
        if use_fallbacks {
            request = request.header("anthropic-beta", FALLBACK_BETA);
        }

        let resp = request.send().await.map_err(|source| ProviderError::Http {
            provider: NAME,
            source,
        })?;
        let v = read_json(NAME, resp).await?;

        match v["stop_reason"].as_str() {
            Some("refusal") => {
                let reason = v["stop_details"]["explanation"]
                    .as_str()
                    .or_else(|| v["stop_details"]["category"].as_str())
                    .unwrap_or("safety classifier declined")
                    .to_string();
                return Err(ProviderError::Refused {
                    provider: NAME,
                    reason,
                });
            }
            Some("max_tokens") => return Err(ProviderError::Truncated { provider: NAME }),
            _ => {}
        }

        let text = v["content"]
            .as_array()
            .and_then(|blocks| blocks.iter().find(|b| b["type"] == "text"))
            .and_then(|b| b["text"].as_str())
            .ok_or_else(|| ProviderError::BadOutput {
                provider: NAME,
                detail: "no text block in response".into(),
            })?;
        parse_json_output(NAME, text)
    }
}
