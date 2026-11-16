//! MinerU multi-key pool for the official mineru.net service.
//!
//! Keys are stored through the existing `models::save_secret` mechanism
//! (`credentials.json`, mode 0600) under per-slot kinds
//! `mineru:official:0`, `mineru:official:1`, … (and `mineru:302:N` for the
//! 302 provider). `SAG_MINERU_API_KEYS` (comma-separated) injects extra keys
//! at runtime; they are merged with stored keys, deduplicated by value, and
//! never written to disk. Real key values never reach logs, API responses
//! or the database — only the slot number and a `****last4` mask.
use super::{
    store::ParseStore,
    types::{mask_key, now_unix},
};
use anyhow::{bail, Context, Result};
use sag_protocol::mineru::MineruProvider;
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

/// Env slots live above the stored range so stats never collide.
pub const ENV_SLOT_BASE: i64 = 1000;
const MAX_STORED_SLOTS: i64 = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeySource {
    Stored,
    Env,
}

#[derive(Clone, Debug)]
pub struct PoolEntry {
    pub slot: i64,
    pub source: KeySource,
    pub key: String,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct MaskedKey {
    pub index: i64,
    pub source: &'static str,
    pub masked: String,
    pub success: i64,
    pub failure: i64,
    pub pages: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cooled_until: Option<i64>,
}

pub fn slot_kind(provider: MineruProvider, slot: i64) -> String {
    format!("{}:{slot}", provider.credential_kind())
}

pub fn provider_name(provider: MineruProvider) -> &'static str {
    match provider {
        MineruProvider::Official => "official",
        MineruProvider::Ai302 => "302",
    }
}

/// All stored keys for a provider, in slot order.
pub fn stored_keys(data_dir: &Path, provider: MineruProvider) -> Result<Vec<(i64, String)>> {
    let mut out = Vec::new();
    for slot in 0..MAX_STORED_SLOTS {
        let kind = slot_kind(provider, slot);
        if crate::models::has_secret(data_dir, &kind)? {
            let key = crate::models::secret(data_dir, &kind)?;
            if !key.trim().is_empty() {
                out.push((slot, key));
            }
        }
    }
    Ok(out)
}

pub fn env_keys() -> Vec<String> {
    std::env::var("SAG_MINERU_API_KEYS")
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|key| !key.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Merged pool: stored slots first (ascending), then env keys, deduplicated
/// by exact value.
pub fn load_pool(data_dir: &Path, provider: MineruProvider) -> Result<Vec<PoolEntry>> {
    let mut pool: Vec<PoolEntry> = stored_keys(data_dir, provider)?
        .into_iter()
        .map(|(slot, key)| PoolEntry {
            slot,
            source: KeySource::Stored,
            key,
        })
        .collect();
    // Env slots are numbered by their position among the env keys that
    // actually join the pool, not by their position in the raw env list
    // (a duplicate that is skipped must not leave a gap).
    let mut env_slot = ENV_SLOT_BASE;
    for key in env_keys() {
        if pool.iter().any(|entry| entry.key == key) {
            continue;
        }
        pool.push(PoolEntry {
            slot: env_slot,
            source: KeySource::Env,
            key,
        });
        env_slot += 1;
    }
    Ok(pool)
}

/// Append a key to the first free stored slot. Returns the slot number.
pub fn save_pool_key(data_dir: &Path, provider: MineruProvider, key: &str) -> Result<i64> {
    if key.trim().is_empty() {
        bail!("MinerU Key 不能为空");
    }
    let used: Vec<i64> = stored_keys(data_dir, provider)?
        .into_iter()
        .map(|(slot, _)| slot)
        .collect();
    let slot = (0..MAX_STORED_SLOTS)
        .find(|slot| !used.contains(slot))
        .context("MinerU Key 槽位已满")?;
    crate::models::save_secret(data_dir, &slot_kind(provider, slot), key.trim())?;
    Ok(slot)
}

pub fn delete_pool_key(data_dir: &Path, provider: MineruProvider, slot: i64) -> Result<()> {
    if !(0..MAX_STORED_SLOTS).contains(&slot) {
        bail!("只能删除已存储的 Key（环境变量注入的 Key 请从环境变量移除）");
    }
    crate::models::clear_secret(data_dir, &slot_kind(provider, slot))
}

/// True when an error message looks like a 429 / quota rejection and the key
/// should be cooled down and rotated.
pub fn is_quota_error(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("429")
        || lower.contains("quota")
        || lower.contains("rate limit")
        || lower.contains("too many requests")
        || message.contains('额')
        || message.contains("余额不足")
}

pub struct KeyPool {
    data_dir: PathBuf,
    provider: MineruProvider,
    cooldown_secs: u64,
    store: Arc<ParseStore>,
    next: Mutex<usize>,
}

impl KeyPool {
    pub fn new(
        data_dir: PathBuf,
        provider: MineruProvider,
        cooldown_secs: u64,
        store: Arc<ParseStore>,
    ) -> Self {
        Self {
            data_dir,
            provider,
            cooldown_secs,
            store,
            next: Mutex::new(0),
        }
    }

    pub fn pool(&self) -> Result<Vec<PoolEntry>> {
        load_pool(&self.data_dir, self.provider)
    }

    pub fn masked_list(&self) -> Result<Vec<MaskedKey>> {
        let name = provider_name(self.provider);
        Ok(self
            .pool()?
            .into_iter()
            .map(|entry| {
                let stat = self.store.key_stat(name, entry.slot).unwrap_or_else(|_| {
                    super::store::KeyStat {
                        provider: name.into(),
                        slot: entry.slot,
                        success: 0,
                        failure: 0,
                        pages: 0,
                        last_error: None,
                        cooled_until: None,
                    }
                });
                MaskedKey {
                    index: entry.slot,
                    source: match entry.source {
                        KeySource::Stored => "stored",
                        KeySource::Env => "env",
                    },
                    masked: mask_key(&entry.key),
                    success: stat.success,
                    failure: stat.failure,
                    pages: stat.pages,
                    cooled_until: stat.cooled_until.filter(|until| *until > now_unix()),
                }
            })
            .collect())
    }

    fn cooled(&self, slot: i64) -> bool {
        self.store
            .key_stat(provider_name(self.provider), slot)
            .map(|stat| stat.cooled_until.is_some_and(|until| until > now_unix()))
            .unwrap_or(false)
    }

    /// Round-robin pick of the next usable key, skipping cooled keys.
    /// Returns `None` when the pool is empty or every key is cooling down.
    pub fn select(&self) -> Result<Option<PoolEntry>> {
        let pool = self.pool()?;
        if pool.is_empty() {
            return Ok(None);
        }
        let mut next = self.next.lock().unwrap();
        for offset in 0..pool.len() {
            let index = (*next + offset) % pool.len();
            let entry = &pool[index];
            if self.cooled(entry.slot) {
                continue;
            }
            *next = (index + 1) % pool.len();
            return Ok(Some(entry.clone()));
        }
        Ok(None)
    }

    /// Record one MinerU attempt outcome for a pool slot. A quota/429
    /// failure cools the key down for `cooldown_secs`.
    pub fn report(&self, slot: i64, success: bool, pages: u32, error: Option<&str>) -> Result<()> {
        let cooled_until =
            if !success && error.is_some_and(is_quota_error) && self.cooldown_secs > 0 {
                Some(now_unix() + self.cooldown_secs as i64)
            } else {
                None
            };
        // A successful attempt clears a stale cooldown.
        let cooled_until = if success { None } else { cooled_until };
        self.store.record_key_outcome(
            provider_name(self.provider),
            slot,
            success,
            pages,
            error,
            cooled_until,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quota_detection_matches_markers_only() {
        assert!(is_quota_error("MinerU 请求失败（HTTP 429）"));
        assert!(is_quota_error("quota exceeded for today"));
        assert!(is_quota_error("Rate Limit exceeded"));
        assert!(is_quota_error("请检查密钥、额度及文件限制"));
        assert!(!is_quota_error("MinerU 解析失败，尝试下一解析器"));
        assert!(!is_quota_error("network timeout"));
    }

    #[test]
    fn pool_merges_stored_and_env_without_duplicates() {
        let dir = tempfile::tempdir().unwrap();
        save_pool_key(dir.path(), MineruProvider::Official, "stored-key-aaaa").unwrap();
        save_pool_key(dir.path(), MineruProvider::Official, "stored-key-bbbb").unwrap();
        std::env::set_var("SAG_MINERU_API_KEYS", "stored-key-aaaa,env-key-cccc");
        let pool = load_pool(dir.path(), MineruProvider::Official).unwrap();
        std::env::remove_var("SAG_MINERU_API_KEYS");
        assert_eq!(pool.len(), 3);
        assert_eq!(pool[0].slot, 0);
        assert_eq!(pool[1].slot, 1);
        assert_eq!(pool[2].source, KeySource::Env);
        assert_eq!(pool[2].slot, ENV_SLOT_BASE);
        // Masked view never contains the key itself.
        let masked = mask_key(&pool[0].key);
        assert!(!masked.contains("stored-key"));
        delete_pool_key(dir.path(), MineruProvider::Official, 1).unwrap();
        assert_eq!(
            stored_keys(dir.path(), MineruProvider::Official)
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn round_robin_skips_cooled_keys() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(ParseStore::open(&dir.path().join("parse.db")).unwrap());
        save_pool_key(dir.path(), MineruProvider::Official, "key-one-1111").unwrap();
        save_pool_key(dir.path(), MineruProvider::Official, "key-two-2222").unwrap();
        let pool = KeyPool::new(
            dir.path().to_path_buf(),
            MineruProvider::Official,
            60,
            store.clone(),
        );
        let first = pool.select().unwrap().unwrap();
        assert_eq!(first.slot, 0);
        pool.report(first.slot, false, 0, Some("HTTP 429 too many requests"))
            .unwrap();
        // Slot 0 is cooling down; the next pick must be slot 1, repeatedly.
        assert_eq!(pool.select().unwrap().unwrap().slot, 1);
        assert_eq!(pool.select().unwrap().unwrap().slot, 1);
        pool.report(1, true, 3, None).unwrap();
        let stat = store.key_stat("official", 1).unwrap();
        assert_eq!((stat.success, stat.pages), (1, 3));
    }
}
