use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct Config {
    pub server: ServerConfig,
    pub cache: CacheConfig,
    pub extraction: ExtractionConfig,
    pub providers: ProvidersConfig,
    /// Alias -> "provider:model-id". Lets callers say `model=haiku` instead of a full id.
    pub models: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct ServerConfig {
    pub bind: String,
    /// Hard cap on an uploaded PDF, enforced while the body streams in.
    pub max_upload_bytes: usize,
    /// Per-request timeout for the upstream model call, in seconds.
    pub request_timeout_secs: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct CacheConfig {
    pub enabled: bool,
    /// Directory for the on-disk cache. Results survive restarts.
    pub dir: String,
    /// 0 means entries never expire.
    pub ttl_secs: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct ExtractionConfig {
    /// Alias or "provider:model" used when a request does not name a model.
    pub default_model: String,
    /// When inferring a schema from a JSON template, allow every leaf to be null
    /// so the model can say "not in the document" instead of inventing a value.
    pub nullable_leaves: bool,
    pub max_output_tokens: u32,
    /// Cap on the plain-text rendering of a spreadsheet or Word upload.
    pub max_text_chars: usize,
    pub prompt: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct ProvidersConfig {
    pub anthropic: AnthropicConfig,
    pub openai: OpenAiConfig,
    pub gemini: GeminiConfig,
    pub deepseek: CompatConfig,
    pub zai: CompatConfig,
    pub xiaomi: CompatConfig,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct AnthropicConfig {
    pub api_key_env: String,
    pub base_url: String,
    /// Opt into Anthropic's server-side refusal fallback (`fallbacks: "default"`).
    /// Only sent for models that support it (not Haiku).
    pub fallbacks: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct OpenAiConfig {
    pub api_key_env: String,
    pub base_url: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct GeminiConfig {
    pub api_key_env: String,
    pub base_url: String,
}

/// An OpenAI-compatible chat-completions provider without file input
/// (DeepSeek, Z.ai, Xiaomi). Documents are sent as text.
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(default)]
pub struct CompatConfig {
    pub api_key_env: String,
    /// Base URL up to, but not including, `/chat/completions`.
    pub base_url: String,
    /// Send `response_format: {"type": "json_object"}`. Turn off for models that reject it.
    pub json_mode: bool,
}

impl CompatConfig {
    fn new(api_key_env: &str, base_url: &str) -> Self {
        Self {
            api_key_env: api_key_env.into(),
            base_url: base_url.into(),
            json_mode: true,
        }
    }
}

impl Default for ProvidersConfig {
    fn default() -> Self {
        Self {
            anthropic: AnthropicConfig::default(),
            openai: OpenAiConfig::default(),
            gemini: GeminiConfig::default(),
            deepseek: CompatConfig::new("DEEPSEEK_API_KEY", "https://api.deepseek.com"),
            zai: CompatConfig::new("ZAI_API_KEY", "https://api.z.ai/api/paas/v4"),
            xiaomi: CompatConfig::new("XIAOMI_API_KEY", "https://api.xiaomimimo.com/v1"),
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        let mut models = BTreeMap::new();
        for (alias, target) in [
            ("flash", "gemini:gemini-3.6-flash"),
            ("flash-lite", "gemini:gemini-3.5-flash-lite"),
            ("luna", "openai:gpt-6-luna"),
            ("nano", "openai:gpt-5-nano"),
            ("haiku", "anthropic:claude-haiku-5-5"),
            ("sonnet", "anthropic:claude-sonnet-5-5"),
            ("deepseek-flash", "deepseek:deepseek-v4-flash"),
            ("glm-flash", "zai:glm-4.7-flash"),
            ("mimo-flash", "xiaomi:mimo-v2.6-flash"),
        ] {
            models.insert(alias.to_string(), target.to_string());
        }
        Self {
            server: ServerConfig::default(),
            cache: CacheConfig::default(),
            extraction: ExtractionConfig::default(),
            providers: ProvidersConfig::default(),
            models,
        }
    }
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: "0.0.0.0:8080".into(),
            max_upload_bytes: 32 * 1024 * 1024,
            request_timeout_secs: 300,
        }
    }
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            dir: ".genparse-cache".into(),
            ttl_secs: 7 * 24 * 3600,
        }
    }
}

impl Default for ExtractionConfig {
    fn default() -> Self {
        Self {
            default_model: "haiku".into(),
            nullable_leaves: true,
            max_output_tokens: 16000,
            max_text_chars: 1_000_000,
            prompt: "Extract the requested fields from the attached document. \
                     Return only JSON that matches the schema. Copy values exactly as they \
                     appear in the document; do not invent data. Use null for any field the \
                     document does not contain."
                .into(),
        }
    }
}

impl Default for AnthropicConfig {
    fn default() -> Self {
        Self {
            api_key_env: "ANTHROPIC_API_KEY".into(),
            base_url: "https://api.anthropic.com".into(),
            fallbacks: true,
        }
    }
}

impl Default for OpenAiConfig {
    fn default() -> Self {
        Self {
            api_key_env: "OPENAI_API_KEY".into(),
            base_url: "https://api.openai.com".into(),
        }
    }
}

impl Default for GeminiConfig {
    fn default() -> Self {
        Self {
            api_key_env: "GEMINI_API_KEY".into(),
            base_url: "https://generativelanguage.googleapis.com".into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderKind {
    Anthropic,
    OpenAi,
    Gemini,
    DeepSeek,
    Zai,
    Xiaomi,
}

impl ProviderKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            ProviderKind::Anthropic => "anthropic",
            ProviderKind::OpenAi => "openai",
            ProviderKind::Gemini => "gemini",
            ProviderKind::DeepSeek => "deepseek",
            ProviderKind::Zai => "zai",
            ProviderKind::Xiaomi => "xiaomi",
        }
    }

    /// Names accepted for this provider in `provider:model` and loose lookups.
    fn names(&self) -> &'static [&'static str] {
        match self {
            ProviderKind::Anthropic => &["anthropic", "claude"],
            ProviderKind::OpenAi => &["openai", "chatgpt", "gpt"],
            ProviderKind::Gemini => &["gemini", "google"],
            ProviderKind::DeepSeek => &["deepseek"],
            ProviderKind::Zai => &["zai", "z.ai", "zhipu", "glm"],
            ProviderKind::Xiaomi => &["xiaomi", "mimo"],
        }
    }

    const ALL: [ProviderKind; 6] = [
        Self::Anthropic,
        Self::OpenAi,
        Self::Gemini,
        Self::DeepSeek,
        Self::Zai,
        Self::Xiaomi,
    ];

    fn parse(s: &str) -> Option<Self> {
        let s = s.to_ascii_lowercase();
        Self::ALL
            .into_iter()
            .find(|p| p.names().contains(&s.as_str()))
    }

    fn infer_from_model_id(model: &str) -> Option<Self> {
        let m = model.to_ascii_lowercase();
        if m.starts_with("claude") {
            Some(Self::Anthropic)
        } else if m.starts_with("gemini") {
            Some(Self::Gemini)
        } else if m.starts_with("gpt")
            || m.starts_with("o1")
            || m.starts_with("o3")
            || m.starts_with("o4")
        {
            Some(Self::OpenAi)
        } else if m.starts_with("deepseek") {
            Some(Self::DeepSeek)
        } else if m.starts_with("glm") {
            Some(Self::Zai)
        } else if m.starts_with("mimo") {
            Some(Self::Xiaomi)
        } else {
            None
        }
    }
}

/// A fully resolved model selection.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct ModelRef {
    pub provider: ProviderKind,
    pub model: String,
}

impl std::fmt::Display for ModelRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.provider.as_str(), self.model)
    }
}

impl Config {
    pub fn load(path: Option<&Path>) -> Result<Self> {
        match path {
            Some(p) => {
                let text = std::fs::read_to_string(p)
                    .with_context(|| format!("reading config file {}", p.display()))?;
                toml::from_str(&text).with_context(|| format!("parsing {}", p.display()))
            }
            None => {
                let default_path = Path::new("config.toml");
                if default_path.exists() {
                    Self::load(Some(default_path))
                } else {
                    Ok(Self::default())
                }
            }
        }
    }

    /// Resolve a user-supplied model string, trying in order:
    /// 1. an alias from `[models]` (exact);
    /// 2. a `provider:model-id` pair, used verbatim;
    /// 3. a loose name such as `gemini-flash3.5`, `claude-haiku` or `xiaomi`,
    ///    matched by its words against the configured aliases and their ids;
    /// 4. a bare model id whose provider is inferred from its prefix.
    pub fn resolve_model(&self, requested: Option<&str>) -> Result<ModelRef> {
        let raw = requested
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(&self.extraction.default_model);

        let target = self.models.get(raw).map(String::as_str).unwrap_or(raw);

        if let Some((p, m)) = target.split_once(':') {
            let provider = ProviderKind::parse(p)
                .with_context(|| format!("unknown provider '{p}' in model '{target}'"))?;
            if m.is_empty() {
                bail!("model id missing in '{target}'");
            }
            return Ok(ModelRef {
                provider,
                model: m.to_string(),
            });
        }

        if let Some(found) = self.loose_match(target)? {
            return Ok(found);
        }

        let provider = ProviderKind::infer_from_model_id(target).with_context(|| {
            format!(
                "cannot determine provider for model '{target}'; use 'provider:model' or add an alias under [models]"
            )
        })?;
        Ok(ModelRef {
            provider,
            model: target.to_string(),
        })
    }

    /// Match `raw` by words against every alias: `gemini-flash3.5` has the words
    /// {gemini, flash, 3, 5}, which are all present in `gemini-3.5-flash-lite`.
    /// Among aliases containing every requested word, the one with the fewest
    /// extra words wins; a tie between different models is an error.
    fn loose_match(&self, raw: &str) -> Result<Option<ModelRef>> {
        let wanted = words(raw);
        if wanted.is_empty() {
            return Ok(None);
        }
        let mut best: Vec<(usize, &str, ModelRef)> = Vec::new();
        for (alias, target) in &self.models {
            let Some(model) = self.parse_target(target) else {
                continue;
            };
            let mut have = words(alias);
            have.extend(words(&model.model));
            have.extend(model.provider.names().iter().map(|s| s.to_string()));
            if !wanted.is_subset(&have) {
                continue;
            }
            let extra = have.len() - wanted.len();
            match best.first() {
                Some((e, ..)) if *e < extra => {}
                Some((e, ..)) if *e == extra => best.push((extra, alias, model)),
                _ => best = vec![(extra, alias, model)],
            }
        }
        best.dedup_by(|a, b| a.2 == b.2);
        match best.as_slice() {
            [] => Ok(None),
            [(_, _, model)] => Ok(Some(model.clone())),
            many => {
                let names: Vec<&str> = many.iter().map(|(_, a, _)| *a).collect();
                bail!(
                    "model '{raw}' is ambiguous: matches {}; use one of those aliases",
                    names.join(", ")
                )
            }
        }
    }

    /// Parse an alias target (`provider:model-id` or bare id). `None` if malformed.
    fn parse_target(&self, target: &str) -> Option<ModelRef> {
        match target.split_once(':') {
            Some((p, m)) if !m.is_empty() => Some(ModelRef {
                provider: ProviderKind::parse(p)?,
                model: m.to_string(),
            }),
            Some(_) => None,
            None => Some(ModelRef {
                provider: ProviderKind::infer_from_model_id(target)?,
                model: target.to_string(),
            }),
        }
    }
}

/// Lower-case alphanumeric words, split on separators and on letter/digit
/// boundaries: `Gemini-Flash3.5` -> {gemini, flash, 3, 5}.
fn words(s: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let mut cur = String::new();
    let mut cur_is_digit = false;
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            let d = c.is_ascii_digit();
            if !cur.is_empty() && d != cur_is_digit {
                out.insert(std::mem::take(&mut cur));
            }
            cur_is_digit = d;
            cur.push(c.to_ascii_lowercase());
        } else if !cur.is_empty() {
            out.insert(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        out.insert(cur);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loose_model_names() {
        let cfg = Config::default();
        let m = |s: &str| cfg.resolve_model(Some(s)).unwrap().to_string();
        assert_eq!(m("gemini-flash3.5"), "gemini:gemini-3.5-flash-lite");
        assert_eq!(m("Gemini Flash"), "gemini:gemini-3.6-flash");
        assert_eq!(m("gemini-flash-lite"), "gemini:gemini-3.5-flash-lite");
        assert_eq!(m("claude-haiku"), "anthropic:claude-haiku-5-5");
        assert_eq!(m("claude_sonnet_5.5"), "anthropic:claude-sonnet-5-5");
        assert_eq!(m("gpt-nano"), "openai:gpt-5-nano");
        assert_eq!(m("xiaomi"), "xiaomi:mimo-v2.6-flash");
        assert_eq!(m("glm"), "zai:glm-4.7-flash");
        assert_eq!(m("deepseek-v4"), "deepseek:deepseek-v4-flash");
        // no alias has all the words -> treated as a verbatim id of the inferred provider
        assert_eq!(m("gemini-flash-9"), "gemini:gemini-flash-9");

        // a single regional variant loses to the plain alias (fewer extra words)...
        let mut two = Config::default();
        two.models
            .insert("sonnet-eu".into(), "anthropic:claude-sonnet-5-5-eu".into());
        assert_eq!(
            two.resolve_model(Some("claude-sonnet")).unwrap().model,
            "claude-sonnet-5-5"
        );
        // ...but two equally close candidates are an error
        two.models.remove("sonnet");
        two.models
            .insert("sonnet-us".into(), "anthropic:claude-sonnet-5-5-us".into());
        assert!(
            two.resolve_model(Some("claude-sonnet"))
                .unwrap_err()
                .to_string()
                .contains("ambiguous")
        );
    }

    #[test]
    fn resolves_aliases_pairs_and_bare_ids() {
        let cfg = Config::default();
        let haiku = cfg.resolve_model(Some("haiku")).unwrap();
        assert_eq!(haiku.provider, ProviderKind::Anthropic);
        assert_eq!(haiku.model, "claude-haiku-5-5");

        let pair = cfg.resolve_model(Some("gemini:gemini-3.6-flash")).unwrap();
        assert_eq!(pair.provider, ProviderKind::Gemini);

        let bare = cfg.resolve_model(Some("gpt-6-luna")).unwrap();
        assert_eq!(bare.provider, ProviderKind::OpenAi);

        let default = cfg.resolve_model(None).unwrap();
        assert_eq!(default.to_string(), "anthropic:claude-haiku-5-5");

        assert!(cfg.resolve_model(Some("mystery-model")).is_err());

        // verbatim ids and provider:model pairs are never loosely matched
        assert_eq!(
            cfg.resolve_model(Some("gemini-3.6-flash-preview"))
                .unwrap()
                .model,
            "gemini-3.6-flash-preview"
        );
        assert_eq!(
            cfg.resolve_model(Some("gemini:anything")).unwrap().model,
            "anything"
        );

        let ds = cfg.resolve_model(Some("deepseek-flash")).unwrap();
        assert_eq!(ds.provider, ProviderKind::DeepSeek);
        assert_eq!(
            cfg.resolve_model(Some("glm-4.7-flash")).unwrap().provider,
            ProviderKind::Zai
        );
        assert_eq!(
            cfg.resolve_model(Some("mimo:mimo-v2.6-flash"))
                .unwrap()
                .provider,
            ProviderKind::Xiaomi
        );
    }
}
