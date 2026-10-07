pub mod anthropic;
pub mod gemini;
pub mod openai;
pub mod openai_compat;

use std::{sync::Arc, time::Duration};

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;
use thiserror::Error;

use crate::config::{Config, ModelRef, ProviderKind};
pub use crate::document::Document;

/// Everything a provider needs to run one extraction.
pub struct ExtractRequest<'a> {
    pub model: &'a str,
    pub filename: &'a str,
    /// The upload: a PDF as base64, or the text rendering of another format.
    pub document: &'a Document,
    pub schema: &'a Value,
    pub prompt: &'a str,
    pub max_output_tokens: u32,
}

#[derive(Debug, Error)]
pub enum ProviderError {
    #[error("API key not set: export {0}")]
    MissingApiKey(String),
    #[error("network error talking to {provider}: {source}")]
    Http {
        provider: &'static str,
        #[source]
        source: reqwest::Error,
    },
    #[error("{provider} returned HTTP {status}: {body}")]
    Api {
        provider: &'static str,
        status: u16,
        body: String,
    },
    #[error("{provider} refused the request: {reason}")]
    Refused {
        provider: &'static str,
        reason: String,
    },
    #[error("{provider} output was cut off at the token limit; raise extraction.max_output_tokens")]
    Truncated { provider: &'static str },
    #[error("{provider} returned something that is not the expected JSON: {detail}")]
    BadOutput {
        provider: &'static str,
        detail: String,
    },
}

impl ProviderError {
    /// HTTP status to surface to our own caller.
    pub fn status_code(&self) -> u16 {
        match self {
            ProviderError::MissingApiKey(_) => 500,
            ProviderError::Http { .. } => 502,
            ProviderError::Api { status, .. } => match *status {
                401 | 403 => 502,
                429 => 429,
                s if s >= 500 => 502,
                _ => 502,
            },
            ProviderError::Refused { .. } => 422,
            ProviderError::Truncated { .. } => 502,
            ProviderError::BadOutput { .. } => 502,
        }
    }
}

#[async_trait]
pub trait Provider: Send + Sync {
    fn name(&self) -> &'static str;
    /// Whether the API takes PDF bytes directly. If not, the PDF's text layer is
    /// extracted locally and sent as `Document::Text`.
    fn accepts_pdf(&self) -> bool {
        true
    }
    async fn extract(&self, req: &ExtractRequest<'_>) -> Result<Value, ProviderError>;
}

/// All configured providers behind one lookup.
pub struct Providers {
    anthropic: Arc<dyn Provider>,
    openai: Arc<dyn Provider>,
    gemini: Arc<dyn Provider>,
    deepseek: Arc<dyn Provider>,
    zai: Arc<dyn Provider>,
    xiaomi: Arc<dyn Provider>,
}

impl Providers {
    pub fn from_config(cfg: &Config) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(cfg.server.request_timeout_secs))
            .connect_timeout(Duration::from_secs(20))
            .user_agent(concat!("genparse/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(Self {
            anthropic: Arc::new(anthropic::Anthropic::new(
                http.clone(),
                cfg.providers.anthropic.clone(),
            )),
            openai: Arc::new(openai::OpenAi::new(
                http.clone(),
                cfg.providers.openai.clone(),
            )),
            gemini: Arc::new(gemini::Gemini::new(
                http.clone(),
                cfg.providers.gemini.clone(),
            )),
            deepseek: Arc::new(openai_compat::OpenAiCompat::new(
                "deepseek",
                http.clone(),
                cfg.providers.deepseek.clone(),
            )),
            zai: Arc::new(openai_compat::OpenAiCompat::new(
                "zai",
                http.clone(),
                cfg.providers.zai.clone(),
            )),
            xiaomi: Arc::new(openai_compat::OpenAiCompat::new(
                "xiaomi",
                http,
                cfg.providers.xiaomi.clone(),
            )),
        })
    }

    pub fn get(&self, model: &ModelRef) -> Arc<dyn Provider> {
        match model.provider {
            ProviderKind::Anthropic => self.anthropic.clone(),
            ProviderKind::OpenAi => self.openai.clone(),
            ProviderKind::Gemini => self.gemini.clone(),
            ProviderKind::DeepSeek => self.deepseek.clone(),
            ProviderKind::Zai => self.zai.clone(),
            ProviderKind::Xiaomi => self.xiaomi.clone(),
        }
    }
}

pub(crate) fn api_key(env_var: &str) -> Result<String, ProviderError> {
    std::env::var(env_var)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ProviderError::MissingApiKey(env_var.to_string()))
}

/// Parse model text as JSON, tolerating a ```json fence some models still emit.
pub(crate) fn parse_json_output(
    provider: &'static str,
    text: &str,
) -> Result<Value, ProviderError> {
    let t = text.trim();
    let t = t
        .strip_prefix("```json")
        .or_else(|| t.strip_prefix("```"))
        .map(|s| s.trim_end_matches("```").trim())
        .unwrap_or(t);
    serde_json::from_str(t).map_err(|e| ProviderError::BadOutput {
        provider,
        detail: format!(
            "{e}; first 200 chars: {}",
            t.chars().take(200).collect::<String>()
        ),
    })
}

/// Read an upstream response, mapping non-2xx to `ProviderError::Api`.
pub(crate) async fn read_json(
    provider: &'static str,
    resp: reqwest::Response,
) -> Result<Value, ProviderError> {
    let status = resp.status();
    let body = resp
        .text()
        .await
        .map_err(|source| ProviderError::Http { provider, source })?;
    if !status.is_success() {
        return Err(ProviderError::Api {
            provider,
            status: status.as_u16(),
            body: body.chars().take(2000).collect(),
        });
    }
    serde_json::from_str(&body).map_err(|e| ProviderError::BadOutput {
        provider,
        detail: format!("response was not JSON: {e}"),
    })
}
