use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, FromRequest, Multipart, Query, Request, State},
    http::{
        HeaderName, HeaderValue, StatusCode,
        header::{CONTENT_DISPOSITION, CONTENT_TYPE},
    },
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};
use tower_http::{cors::CorsLayer, trace::TraceLayer};
use tracing::{error, warn};

use crate::{
    cache::{StreamingHasher, sha256_hex},
    extract::{self, App, ExtractError, ExtractInput},
    providers::ProviderError,
};

/// The single-page browser UI, compiled into the binary.
const UI_HTML: &str = include_str!("../ui/index.html");

/// `ui` adds `GET /ui` (and redirects `/` to it).
pub fn router(app: Arc<App>, ui: bool) -> Router {
    // Leave headroom for the non-file multipart fields; the handler enforces the exact file cap.
    let body_limit = app.cfg.server.max_upload_bytes + 1024 * 1024;
    let mut router = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/models", get(models))
        .route("/parse", post(parse))
        .route("/cache/clear", post(clear_cache));
    if ui {
        router = router
            .route("/ui", get(|| async { Html(UI_HTML) }))
            .route("/", get(|| async { Redirect::temporary("/ui") }));
    }
    router
        .layer(DefaultBodyLimit::max(body_limit))
        .layer(CorsLayer::permissive())
        .layer(TraceLayer::new_for_http())
        .with_state(app)
}

pub async fn serve(app: Arc<App>, ui: bool) -> anyhow::Result<()> {
    let bind = app.cfg.server.bind.clone();
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    tracing::info!(%bind, cache = app.cache.enabled(), default_model = %app.cfg.extraction.default_model, ui, "genparse listening");
    if ui {
        tracing::info!(
            "browser UI at http://{}/ui",
            bind.replace("0.0.0.0", "localhost")
        );
    }
    axum::serve(listener, router(app, ui))
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

async fn models(State(app): State<Arc<App>>) -> Json<Value> {
    let aliases: Vec<Value> = app
        .cfg
        .models
        .iter()
        .map(|(alias, target)| json!({ "alias": alias, "target": target }))
        .collect();
    Json(json!({ "default": app.cfg.extraction.default_model, "aliases": aliases }))
}

async fn clear_cache(State(app): State<Arc<App>>) -> Result<Json<Value>, ApiError> {
    let removed = app
        .cache
        .clear()
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?;
    Ok(Json(json!({ "cleared": removed })))
}

/// Query-string alternatives to the `x-genparse-*` headers for raw-body uploads.
#[derive(Deserialize, Default)]
struct ParseQuery {
    schema: Option<String>,
    model: Option<String>,
    nocache: Option<String>,
    filename: Option<String>,
}

struct Upload {
    bytes: Vec<u8>,
    sha256: String,
    filename: String,
    schema_text: String,
    model: Option<String>,
    bypass_cache: bool,
}

/// Two request shapes are accepted.
///
/// `multipart/form-data` with:
/// - `file`   the document: PDF, spreadsheet or .docx (required). Streamed and hashed chunk by chunk.
/// - `schema` the entities to extract, as JSON text or a .json file part (required).
/// - `model`  alias, loose name, `provider:model`, or bare model id (optional).
/// - `nocache` "1"/"true" to skip the cache for this request (optional).
///
/// Or the file bytes as the raw body, typed by `Content-Type` (`application/pdf`,
/// the xlsx/xls/ods/docx types, or `application/octet-stream` to sniff), with
/// `x-genparse-schema` (required), `x-genparse-model`, `x-genparse-nocache` and
/// `x-genparse-filename` headers, or the same as `?schema=`, `?model=`,
/// `?nocache=`, `?filename=` query parameters.
async fn parse(
    State(app): State<Arc<App>>,
    Query(query): Query<ParseQuery>,
    req: Request,
) -> Result<Response, ApiError> {
    let max = app.cfg.server.max_upload_bytes;
    let content_type = req
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();

    let upload = if content_type.starts_with("multipart/form-data") {
        let multipart = Multipart::from_request(req, &app)
            .await
            .map_err(|e| ApiError::bad_request(format!("malformed multipart body: {e}")))?;
        read_multipart(multipart, max).await?
    } else {
        read_raw_body(req, max, &content_type, query).await?
    };

    let schema_input: Value = serde_json::from_str(&upload.schema_text)
        .map_err(|e| ApiError::bad_request(format!("schema is not valid JSON: {e}")))?;

    let out = extract::extract(
        &app,
        ExtractInput {
            file: &upload.bytes,
            file_sha256: upload.sha256,
            filename: &upload.filename,
            schema_input: &schema_input,
            model: upload.model.as_deref(),
            bypass_cache: upload.bypass_cache,
        },
    )
    .await?;

    let mut resp = Json(out.result).into_response();
    let h = resp.headers_mut();
    h.insert(
        HeaderName::from_static("x-genparse-cache"),
        HeaderValue::from_static(out.cache.as_str()),
    );
    if let Ok(v) = HeaderValue::from_str(&out.model.to_string()) {
        h.insert(HeaderName::from_static("x-genparse-model"), v);
    }
    if let Ok(v) = HeaderValue::from_str(&out.file_sha256) {
        h.insert(HeaderName::from_static("x-genparse-file-sha256"), v);
    }
    h.insert(
        HeaderName::from_static("x-genparse-file-type"),
        HeaderValue::from_static(out.kind.as_str()),
    );
    Ok(resp)
}

fn truthy(v: &str) -> bool {
    matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes")
}

async fn read_multipart(mut multipart: Multipart, max: usize) -> Result<Upload, ApiError> {
    let mut file: Option<(Vec<u8>, String, String)> = None;
    let mut schema_text: Option<String> = None;
    let mut model: Option<String> = None;
    let mut bypass_cache = false;

    while let Some(mut field) = multipart
        .next_field()
        .await
        .map_err(|e| ApiError::bad_request(format!("malformed multipart body: {e}")))?
    {
        match field.name().unwrap_or("") {
            "file" | "pdf" | "document" => {
                let filename = field.file_name().unwrap_or("document.pdf").to_string();
                let mut buf: Vec<u8> = Vec::new();
                let mut hasher = StreamingHasher::new();
                while let Some(chunk) = field
                    .chunk()
                    .await
                    .map_err(|e| ApiError::bad_request(format!("upload interrupted: {e}")))?
                {
                    if buf.len() + chunk.len() > max {
                        return Err(ApiError::too_large(max));
                    }
                    hasher.update(&chunk);
                    buf.extend_from_slice(&chunk);
                }
                file = Some((buf, hasher.finish(), filename));
            }
            "schema" | "entities" | "template" => {
                schema_text =
                    Some(field.text().await.map_err(|e| {
                        ApiError::bad_request(format!("reading schema field: {e}"))
                    })?);
            }
            "model" => {
                model = Some(
                    field
                        .text()
                        .await
                        .map_err(|e| ApiError::bad_request(format!("reading model field: {e}")))?,
                );
            }
            "nocache" | "no_cache" => {
                bypass_cache = truthy(&field.text().await.unwrap_or_default());
            }
            other => warn!(field = other, "ignoring unknown multipart field"),
        }
    }

    let (bytes, sha256, filename) =
        file.ok_or_else(|| ApiError::bad_request("missing multipart field 'file' (the document)"))?;
    let schema_text = schema_text.ok_or_else(|| {
        ApiError::bad_request("missing multipart field 'schema' (JSON of the entities to extract)")
    })?;
    Ok(Upload {
        bytes,
        sha256,
        filename,
        schema_text,
        model: model.filter(|m| !m.trim().is_empty()),
        bypass_cache,
    })
}

async fn read_raw_body(
    req: Request,
    max: usize,
    content_type: &str,
    query: ParseQuery,
) -> Result<Upload, ApiError> {
    let (parts, body) = req.into_parts();
    let header = |name: &'static str| {
        parts
            .headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    };
    let schema_text = header("x-genparse-schema")
        .or(query.schema)
        .ok_or_else(|| {
            ApiError::bad_request(
                "missing schema: send the JSON in the x-genparse-schema header or the ?schema= \
                 query parameter (or use multipart/form-data with 'file' and 'schema' fields)",
            )
        })?;
    let model = header("x-genparse-model").or(query.model);
    let bypass_cache = header("x-genparse-nocache")
        .or(query.nocache)
        .is_some_and(|v| truthy(&v));
    let filename = header("x-genparse-filename")
        .or(query.filename)
        .or_else(|| disposition_filename(&parts.headers))
        .unwrap_or_else(|| default_filename(content_type));

    let bytes = axum::body::to_bytes(body, max).await.map_err(|e| {
        if e.to_string().contains("length limit") {
            ApiError::too_large(max)
        } else {
            ApiError::bad_request(format!("reading request body: {e}"))
        }
    })?;
    if bytes.is_empty() {
        return Err(ApiError::bad_request(
            "empty body: send the file bytes as the request body with its Content-Type, or use \
             multipart/form-data",
        ));
    }
    let sha256 = sha256_hex(&bytes);
    Ok(Upload {
        bytes: bytes.to_vec(),
        sha256,
        filename,
        schema_text,
        model,
        bypass_cache,
    })
}

/// `filename="x.pdf"` or `filename=x.pdf` from a Content-Disposition header.
fn disposition_filename(headers: &axum::http::HeaderMap) -> Option<String> {
    let cd = headers.get(CONTENT_DISPOSITION)?.to_str().ok()?;
    cd.split(';').map(str::trim).find_map(|part| {
        let v = part.strip_prefix("filename=")?;
        let v = v.trim().trim_matches('"');
        (!v.is_empty()).then(|| v.to_string())
    })
}

/// A placeholder name whose extension follows the declared media type, so that
/// formats which cannot be told apart by their bytes (.xls vs .doc) still resolve.
fn default_filename(content_type: &str) -> String {
    let mime = content_type.split(';').next().unwrap_or("").trim();
    let ext = match mime {
        "application/pdf" => "pdf",
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet" => "xlsx",
        "application/vnd.ms-excel.sheet.macroenabled.12" => "xlsm",
        "application/vnd.ms-excel.sheet.binary.macroenabled.12" => "xlsb",
        "application/vnd.ms-excel" => "xls",
        "application/vnd.oasis.opendocument.spreadsheet" => "ods",
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document" => "docx",
        "application/msword" => "doc",
        _ => return "document".to_string(),
    };
    format!("document.{ext}")
}

pub struct ApiError {
    status: StatusCode,
    body: Value,
}

impl ApiError {
    fn new(status: StatusCode, msg: impl Into<String>) -> Self {
        Self {
            status,
            body: json!({ "error": msg.into() }),
        }
    }
    fn bad_request(msg: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, msg)
    }
    fn internal(msg: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, msg)
    }
    fn too_large(max: usize) -> Self {
        Self::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            format!("file exceeds the {max}-byte limit"),
        )
    }
}

impl From<ExtractError> for ApiError {
    fn from(e: ExtractError) -> Self {
        match e {
            ExtractError::BadRequest(m) => ApiError::bad_request(m),
            ExtractError::TooLarge(m) => ApiError::new(StatusCode::PAYLOAD_TOO_LARGE, m),
            ExtractError::Provider(p) => {
                let status =
                    StatusCode::from_u16(p.status_code()).unwrap_or(StatusCode::BAD_GATEWAY);
                let kind = match &p {
                    ProviderError::MissingApiKey(_) => "missing_api_key",
                    ProviderError::Http { .. } => "upstream_unreachable",
                    ProviderError::Api { .. } => "upstream_error",
                    ProviderError::Refused { .. } => "refused",
                    ProviderError::Truncated { .. } => "truncated",
                    ProviderError::BadOutput { .. } => "bad_output",
                };
                if status.is_server_error() {
                    error!(error = %p, "provider failure");
                }
                ApiError {
                    status,
                    body: json!({ "error": p.to_string(), "kind": kind }),
                }
            }
            ExtractError::Internal(e) => {
                error!(error = %e, "internal failure");
                ApiError::internal(e.to_string())
            }
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut r = (self.status, Json(self.body)).into_response();
        r.headers_mut()
            .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        r
    }
}
