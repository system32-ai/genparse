//! Providers that expose an OpenAI-compatible `POST /chat/completions`
//! endpoint but take no file input: DeepSeek, Z.ai (GLM) and Xiaomi (MiMo).
//!
//! Documents reach these models as plain text (PDFs are text-extracted
//! locally first), and structured output is best-effort: the schema is placed
//! in the system prompt and JSON mode is requested where supported.

use async_trait::async_trait;
use serde_json::{Value, json};

use super::{
    Document, ExtractRequest, Provider, ProviderError, api_key, parse_json_output, read_json,
};
use crate::config::CompatConfig;

pub struct OpenAiCompat {
    name: &'static str,
    http: reqwest::Client,
    cfg: CompatConfig,
}

impl OpenAiCompat {
    pub fn new(name: &'static str, http: reqwest::Client, cfg: CompatConfig) -> Self {
        Self { name, http, cfg }
    }
}

#[async_trait]
impl Provider for OpenAiCompat {
    fn name(&self) -> &'static str {
        self.name
    }

    fn accepts_pdf(&self) -> bool {
        false
    }

    async fn extract(&self, req: &ExtractRequest<'_>) -> Result<Value, ProviderError> {
        let key = api_key(&self.cfg.api_key_env)?;
        let text = match req.document {
            Document::Text { text } => text,
            Document::Pdf { .. } => {
                return Err(ProviderError::BadOutput {
                    provider: self.name,
                    detail: "received a PDF but only accepts text (internal routing bug)".into(),
                });
            }
        };

        let system = format!(
            "You extract structured data from documents. Respond with a single JSON object \
             and nothing else. The object must conform exactly to this JSON Schema:\n{}",
            serde_json::to_string(req.schema).unwrap_or_default()
        );
        let user = format!("{}\n\nDocument \"{}\":\n\n{text}", req.prompt, req.filename);
        let mut body = json!({
            "model": req.model,
            "max_tokens": req.max_output_tokens,
            "temperature": 0,
            "messages": [
                { "role": "system", "content": system },
                { "role": "user", "content": user }
            ]
        });
        if self.cfg.json_mode {
            body["response_format"] = json!({ "type": "json_object" });
        }

        let resp = self
            .http
            .post(format!(
                "{}/chat/completions",
                self.cfg.base_url.trim_end_matches('/')
            ))
            .bearer_auth(key)
            .json(&body)
            .send()
            .await
            .map_err(|source| ProviderError::Http {
                provider: self.name,
                source,
            })?;
        let v = read_json(self.name, resp).await?;

        let choice = v["choices"]
            .as_array()
            .and_then(|c| c.first())
            .ok_or_else(|| ProviderError::BadOutput {
                provider: self.name,
                detail: "no choices in response".into(),
            })?;
        match choice["finish_reason"].as_str() {
            Some("length") => {
                return Err(ProviderError::Truncated {
                    provider: self.name,
                });
            }
            Some("content_filter") => {
                return Err(ProviderError::Refused {
                    provider: self.name,
                    reason: "content filter".into(),
                });
            }
            _ => {}
        }
        let text = choice["message"]["content"]
            .as_str()
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| ProviderError::BadOutput {
                provider: self.name,
                detail: "empty message content".into(),
            })?;
        parse_json_output(self.name, text)
    }
}
