//! Content-addressed result cache.
//!
//! Key = sha256(pdf bytes) ⊕ provider ⊕ model ⊕ canonical schema ⊕ prompt, so
//! re-uploading the same PDF with the same extraction request returns the
//! stored JSON without calling a model. Entries live in memory and on disk, so
//! they survive restarts. Disabled entirely with `[cache] enabled = false`.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::sync::RwLock;

use crate::config::{CacheConfig, ModelRef};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheEntry {
    pub stored_at: u64,
    pub model: String,
    pub pdf_sha256: String,
    pub result: Value,
}

pub struct Cache {
    enabled: bool,
    dir: PathBuf,
    ttl: Option<Duration>,
    mem: RwLock<HashMap<String, Arc<CacheEntry>>>,
}

impl Cache {
    pub fn new(cfg: &CacheConfig) -> Result<Self> {
        let dir = PathBuf::from(&cfg.dir);
        if cfg.enabled {
            std::fs::create_dir_all(&dir)
                .with_context(|| format!("creating cache dir {}", dir.display()))?;
        }
        Ok(Self {
            enabled: cfg.enabled,
            dir,
            ttl: (cfg.ttl_secs > 0).then(|| Duration::from_secs(cfg.ttl_secs)),
            mem: RwLock::new(HashMap::new()),
        })
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Derive the lookup key for one extraction request.
    pub fn key(pdf_sha256: &str, model: &ModelRef, schema: &Value, prompt: &str) -> String {
        let mut h = Sha256::new();
        h.update(b"genparse-v1\0");
        h.update(pdf_sha256.as_bytes());
        h.update(b"\0");
        h.update(model.to_string().as_bytes());
        h.update(b"\0");
        // serde_json::Value objects are BTreeMaps, so this is key-sorted and canonical.
        h.update(serde_json::to_string(schema).unwrap_or_default().as_bytes());
        h.update(b"\0");
        h.update(prompt.as_bytes());
        hex::encode(h.finalize())
    }

    pub async fn get(&self, key: &str) -> Option<Arc<CacheEntry>> {
        if !self.enabled {
            return None;
        }
        if let Some(e) = self
            .mem
            .read()
            .await
            .get(key)
            .cloned()
            .filter(|e| self.is_fresh(e))
        {
            return Some(e);
        }
        let path = self.path_for(key);
        let bytes = tokio::fs::read(&path).await.ok()?;
        let entry: CacheEntry = serde_json::from_slice(&bytes).ok()?;
        if !self.is_fresh(&entry) {
            let _ = tokio::fs::remove_file(&path).await;
            self.mem.write().await.remove(key);
            return None;
        }
        let entry = Arc::new(entry);
        self.mem
            .write()
            .await
            .insert(key.to_string(), entry.clone());
        Some(entry)
    }

    pub async fn put(&self, key: &str, entry: CacheEntry) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }
        let bytes = serde_json::to_vec(&entry)?;
        let path = self.path_for(key);
        let tmp = path.with_extension("tmp");
        tokio::fs::write(&tmp, &bytes)
            .await
            .with_context(|| format!("writing {}", tmp.display()))?;
        tokio::fs::rename(&tmp, &path)
            .await
            .with_context(|| format!("renaming into {}", path.display()))?;
        self.mem
            .write()
            .await
            .insert(key.to_string(), Arc::new(entry));
        Ok(())
    }

    /// Drop every entry, in memory and on disk. Returns how many files were removed.
    pub async fn clear(&self) -> Result<usize> {
        self.mem.write().await.clear();
        if !self.enabled {
            return Ok(0);
        }
        let mut n = 0;
        let mut rd = tokio::fs::read_dir(&self.dir).await?;
        while let Some(ent) = rd.next_entry().await? {
            let p = ent.path();
            if p.extension().and_then(|e| e.to_str()) == Some("json") {
                tokio::fs::remove_file(&p).await?;
                n += 1;
            }
        }
        Ok(n)
    }

    fn is_fresh(&self, e: &CacheEntry) -> bool {
        match self.ttl {
            None => true,
            Some(ttl) => now_secs().saturating_sub(e.stored_at) <= ttl.as_secs(),
        }
    }

    fn path_for(&self, key: &str) -> PathBuf {
        self.dir.join(format!("{key}.json"))
    }
}

pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Incremental SHA-256 used while an upload streams in.
pub struct StreamingHasher(Sha256);

impl StreamingHasher {
    pub fn new() -> Self {
        Self(Sha256::new())
    }
    pub fn update(&mut self, chunk: &[u8]) {
        self.0.update(chunk);
    }
    pub fn finish(self) -> String {
        hex::encode(self.0.finalize())
    }
}

impl Default for StreamingHasher {
    fn default() -> Self {
        Self::new()
    }
}

#[allow(dead_code)]
pub fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

#[allow(dead_code)]
pub fn cache_dir_exists(p: &Path) -> bool {
    p.is_dir()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ProviderKind;
    use serde_json::json;

    fn model() -> ModelRef {
        ModelRef {
            provider: ProviderKind::Anthropic,
            model: "claude-haiku-5-5".into(),
        }
    }

    #[tokio::test]
    async fn roundtrip_and_disabled() {
        let dir = std::env::temp_dir().join(format!("genparse-cache-test-{}", std::process::id()));
        let cfg = CacheConfig {
            enabled: true,
            dir: dir.to_string_lossy().into_owned(),
            ttl_secs: 0,
        };
        let cache = Cache::new(&cfg).unwrap();
        let key = Cache::key("abc", &model(), &json!({"a": 1}), "p");
        assert!(cache.get(&key).await.is_none());
        cache
            .put(
                &key,
                CacheEntry {
                    stored_at: now_secs(),
                    model: model().to_string(),
                    pdf_sha256: "abc".into(),
                    result: json!({"a": 1}),
                },
            )
            .await
            .unwrap();
        assert_eq!(cache.get(&key).await.unwrap().result, json!({"a": 1}));

        // A second instance over the same dir reads it back from disk.
        let cache2 = Cache::new(&cfg).unwrap();
        assert!(cache2.get(&key).await.is_some());
        assert_eq!(cache2.clear().await.unwrap(), 1);
        assert!(cache2.get(&key).await.is_none());

        let off = Cache::new(&CacheConfig {
            enabled: false,
            ..cfg.clone()
        })
        .unwrap();
        off.put(
            &key,
            CacheEntry {
                stored_at: now_secs(),
                model: "m".into(),
                pdf_sha256: "x".into(),
                result: json!(1),
            },
        )
        .await
        .unwrap();
        assert!(off.get(&key).await.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn key_changes_with_inputs() {
        let base = Cache::key("pdf", &model(), &json!({"a": 1}), "p");
        assert_ne!(base, Cache::key("other", &model(), &json!({"a": 1}), "p"));
        assert_ne!(base, Cache::key("pdf", &model(), &json!({"a": 2}), "p"));
        assert_ne!(base, Cache::key("pdf", &model(), &json!({"a": 1}), "q"));
        // Key order in the schema does not matter.
        assert_eq!(
            Cache::key("pdf", &model(), &json!({"b": 1, "a": 1}), "p"),
            Cache::key("pdf", &model(), &json!({"a": 1, "b": 1}), "p")
        );
    }
}
