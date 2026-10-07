//! The provider-agnostic extraction pipeline shared by the HTTP server and CLI.

use std::sync::Arc;

use serde::Serialize;
use serde_json::Value;
use thiserror::Error;
use tracing::{info, instrument};

use crate::{
    cache::{Cache, CacheEntry, now_secs},
    config::{Config, ModelRef},
    document::{self, ConvertError, DocumentKind},
    providers::{ExtractRequest, ProviderError, Providers},
    schema,
};

pub struct App {
    pub cfg: Config,
    pub cache: Cache,
    pub providers: Providers,
}

impl App {
    pub fn new(cfg: Config) -> anyhow::Result<Arc<Self>> {
        let cache = Cache::new(&cfg.cache)?;
        let providers = Providers::from_config(&cfg)?;
        Ok(Arc::new(Self {
            cfg,
            cache,
            providers,
        }))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CacheStatus {
    Hit,
    Miss,
    Bypass,
    Disabled,
}

impl CacheStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            CacheStatus::Hit => "hit",
            CacheStatus::Miss => "miss",
            CacheStatus::Bypass => "bypass",
            CacheStatus::Disabled => "disabled",
        }
    }
}

pub struct Extraction {
    pub result: Value,
    pub model: ModelRef,
    pub cache: CacheStatus,
    pub file_sha256: String,
    pub kind: DocumentKind,
}

#[derive(Debug, Error)]
pub enum ExtractError {
    #[error("{0}")]
    BadRequest(String),
    /// The upload or its text rendering is over a configured limit.
    #[error("{0}")]
    TooLarge(String),
    #[error(transparent)]
    Provider(#[from] ProviderError),
    #[error("internal error: {0}")]
    Internal(#[from] anyhow::Error),
}

pub struct ExtractInput<'a> {
    /// The uploaded file: PDF, spreadsheet or .docx.
    pub file: &'a [u8],
    pub file_sha256: String,
    pub filename: &'a str,
    /// The caller's entity list: a JSON template or a JSON Schema.
    pub schema_input: &'a Value,
    pub model: Option<&'a str>,
    pub bypass_cache: bool,
}

#[instrument(skip_all, fields(file = %input.filename, bytes = input.file.len()))]
pub async fn extract(app: &App, input: ExtractInput<'_>) -> Result<Extraction, ExtractError> {
    let kind = document::detect(input.file, input.filename).map_err(ExtractError::BadRequest)?;
    let model = app
        .cfg
        .resolve_model(input.model)
        .map_err(|e| ExtractError::BadRequest(e.to_string()))?;
    let schema = schema::build_schema(input.schema_input, app.cfg.extraction.nullable_leaves)
        .map_err(ExtractError::BadRequest)?;
    let prompt = &app.cfg.extraction.prompt;

    let key = Cache::key(&input.file_sha256, &model, &schema, prompt);
    let cache_status = if !app.cache.enabled() {
        CacheStatus::Disabled
    } else if input.bypass_cache {
        CacheStatus::Bypass
    } else if let Some(hit) = app.cache.get(&key).await {
        info!(model = %model, "cache hit");
        return Ok(Extraction {
            result: hit.result.clone(),
            model,
            cache: CacheStatus::Hit,
            file_sha256: input.file_sha256,
            kind,
        });
    } else {
        CacheStatus::Miss
    };

    let provider = app.providers.get(&model);
    let doc = document::convert(
        kind,
        input.file,
        app.cfg.extraction.max_text_chars,
        !provider.accepts_pdf(),
    )
    .map_err(|e| match e {
        ConvertError::TooLarge { .. } => ExtractError::TooLarge(e.to_string()),
        ConvertError::Unreadable(_) => ExtractError::BadRequest(e.to_string()),
    })?;
    info!(model = %model, provider = provider.name(), kind = kind.as_str(), "calling model");
    let result = provider
        .extract(&ExtractRequest {
            model: &model.model,
            filename: input.filename,
            document: &doc,
            schema: &schema,
            prompt,
            max_output_tokens: app.cfg.extraction.max_output_tokens,
        })
        .await?;

    if app.cache.enabled() {
        app.cache
            .put(
                &key,
                CacheEntry {
                    stored_at: now_secs(),
                    model: model.to_string(),
                    pdf_sha256: input.file_sha256.clone(),
                    result: result.clone(),
                },
            )
            .await?;
    }
    Ok(Extraction {
        result,
        model,
        cache: cache_status,
        file_sha256: input.file_sha256,
        kind,
    })
}
