mod cache;
mod config;
mod document;
mod extract;
mod providers;
mod schema;
mod server;

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

use crate::{
    cache::sha256_hex,
    config::Config,
    extract::{App, ExtractInput},
};

/// genparse: fill a JSON template with values extracted from a PDF, spreadsheet
/// or Word document, using the model of your choice (Gemini Flash, OpenAI Luna/Nano, Claude Haiku/Sonnet/Opus/Fable).
#[derive(Parser)]
#[command(version, about)]
struct Cli {
    /// Path to config.toml (defaults to ./config.toml if present, else built-in defaults).
    #[arg(short, long, global = true, env = "GENPARSE_CONFIG")]
    config: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the HTTP service. Flags override the corresponding config keys.
    Serve {
        /// Also serve the browser UI at /ui (and redirect / to it).
        #[arg(long)]
        ui: bool,
        /// Address to listen on (server.bind).
        #[arg(long, value_name = "ADDR")]
        bind: Option<String>,
        /// Default model: alias, loose name such as `gemini-flash3.5` or `claude-haiku`,
        /// `provider:model`, or bare id (extraction.default_model).
        #[arg(short, long)]
        model: Option<String>,
        /// Enable or disable the result cache (cache.enabled), e.g. `--cache=false`.
        #[arg(long, value_name = "BOOL")]
        cache: Option<bool>,
    },
    /// Extract once from the command line and print the filled JSON.
    Extract {
        /// The document to read: PDF, xlsx/xls/ods or docx.
        file: PathBuf,
        /// JSON file with the entities to extract (template or JSON Schema).
        #[arg(short, long)]
        schema: PathBuf,
        /// Model: alias, loose name such as `gemini-flash3.5`, `provider:model`, or bare id.
        #[arg(short, long)]
        model: Option<String>,
        /// Skip the cache for this run.
        #[arg(long)]
        no_cache: bool,
    },
    /// Print the effective configuration as TOML.
    Config,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("genparse=info,tower_http=info")),
        )
        .with_target(false)
        .init();

    let cli = Cli::parse();
    let mut cfg = Config::load(cli.config.as_deref())?;

    match cli.cmd {
        Command::Serve {
            ui,
            bind,
            model,
            cache,
        } => {
            if let Some(bind) = bind {
                cfg.server.bind = bind;
            }
            if let Some(model) = model {
                cfg.extraction.default_model = model;
            }
            if let Some(cache) = cache {
                cfg.cache.enabled = cache;
            }
            cfg.resolve_model(None)
                .map_err(|e| anyhow::anyhow!("default model: {e}"))?;
            let app = App::new(cfg)?;
            server::serve(app, ui).await
        }
        Command::Extract {
            file,
            schema,
            model,
            no_cache,
        } => {
            let app = App::new(cfg)?;
            let bytes =
                std::fs::read(&file).with_context(|| format!("reading {}", file.display()))?;
            let schema_text = std::fs::read_to_string(&schema)
                .with_context(|| format!("reading {}", schema.display()))?;
            let schema_input =
                serde_json::from_str(&schema_text).context("schema file is not valid JSON")?;
            let filename = file
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("document.pdf");
            let out = extract::extract(
                &app,
                ExtractInput {
                    file: &bytes,
                    file_sha256: sha256_hex(&bytes),
                    filename,
                    schema_input: &schema_input,
                    model: model.as_deref(),
                    bypass_cache: no_cache,
                },
            )
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
            eprintln!(
                "model={} cache={} type={}",
                out.model,
                out.cache.as_str(),
                out.kind.as_str()
            );
            println!("{}", serde_json::to_string_pretty(&out.result)?);
            Ok(())
        }
        Command::Config => {
            println!("{}", toml::to_string_pretty(&cfg)?);
            Ok(())
        }
    }
}
