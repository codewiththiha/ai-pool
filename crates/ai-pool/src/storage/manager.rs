//! The `KeyManager`: the administrative surface over the vault and the live
//! pool. Everything it returns by default is censored; plaintext keys only
//! leave the vault through the one method that exists for that purpose.

use std::sync::Arc;

use secrecy::SecretString;
use serde::{Deserialize, Serialize};

use super::{KeyHealth, KeyStatus, Vault};
use crate::engine::KeyPool;
use crate::error::AiError;
use crate::quota::{KeyLimits, KeyQuotaInfo};

/// Censored key info, safe to serialize and expose to untrusted layers
/// such as a UI process or a log line. Never contains plaintext secrets.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyMetadata {
    /// Stable key id.
    pub id: String,
    /// e.g. `sk-proj-...8f92`
    pub censored_key: String,
    /// Live health snapshot (flattened into the JSON object).
    #[serde(flatten)]
    pub status: KeyStatus,
    /// Live quota snapshot: windows (used/remaining/`resets_in_ms`),
    /// in-flight count, and concurrency cap. `None` for unlimited idle keys.
    pub quota: Option<KeyQuotaInfo>,
    /// Unix ms creation timestamp.
    pub created_at_ms: i64,
}

/// Developer control surface over the vault + live pool.
#[derive(Clone)]
pub struct KeyManager {
    vault: Vault,
    pool: Arc<KeyPool>,
}

impl KeyManager {
    pub(crate) const fn new(vault: Vault, pool: Arc<KeyPool>) -> Self {
        Self { vault, pool }
    }

    // ------------------------------------------------------------------
    // Listing
    // ------------------------------------------------------------------

    /// Returns every key with censored display text, live health, and quota
    /// usage. The result contains no secrets and can be serialized as-is.
    pub async fn list_keys(&self) -> Result<Vec<KeyMetadata>, AiError> {
        let records = self.vault.store().load_all().await?;
        Ok(records
            .into_iter()
            .map(|rec| {
                // Prefer the live in-RAM health (cooldowns tick in real time).
                let health = self.pool.health_of(&rec.id).unwrap_or(rec.health);
                let quota = self.pool.quota_of(&rec.id).filter(|q| {
                    q.minute.is_some() || q.hour.is_some() || q.max_concurrency.is_some()
                        || q.in_flight > 0
                });
                KeyMetadata {
                    id: rec.id,
                    censored_key: rec.censored,
                    status: KeyStatus::from(&health),
                    quota,
                    created_at_ms: rec.created_at_ms,
                }
            })
            .collect())
    }

    // ------------------------------------------------------------------
    // Explicit decryption
    // ------------------------------------------------------------------

    /// Decrypts a single key on demand, for the rare flows that genuinely
    /// need the plaintext (say, letting a user view or edit a stored key).
    /// The value comes back wrapped in [`SecretString`]; call
    /// `.expose_secret()` only at the last possible moment.
    pub async fn get_decrypted_key(&self, id: &str) -> Result<SecretString, AiError> {
        Ok(self.vault.decrypt_one(id).await?)
    }

    // ------------------------------------------------------------------
    // Key management
    // ------------------------------------------------------------------

    /// Encrypts and stores a new key, loads it into the live pool, and
    /// returns its generated (deterministic) id.
    pub async fn add_key(&self, plaintext_key: &str) -> Result<String, AiError> {
        self.add_key_with_limits(plaintext_key, None).await
    }

    /// Like [`Self::add_key`], with explicit per-key limits
    /// (`None` = pool default).
    pub async fn add_key_with_limits(
        &self,
        plaintext_key: &str,
        limits: Option<KeyLimits>,
    ) -> Result<String, AiError> {
        if limits.is_some_and(|l| l.max_concurrency.is_some())
            && self.pool.mode() == crate::config::RotationMode::Proactive
        {
            return Err(AiError::InvalidConfig(
                "max_concurrency limits require RotationMode::Reactive;                  proactive mode already spreads load"
                    .into(),
            ));
        }
        let id = crate::crypto::deterministic_id(plaintext_key);
        let secret = SecretString::from(plaintext_key.to_string());
        let record = self.vault.insert_plain(&id, &secret, limits).await?;
        self.pool.upsert(
            crate::engine::KeySeed::new(record.id, secret, record.censored)
                .limits(record.limits),
        );
        Ok(id)
    }

    /// Live quota snapshot for one key: minute/hour window usage,
    /// `resets_at_ms` / `resets_in_ms` for realtime countdowns, in-flight
    /// count, and the concurrency cap.
    pub fn key_quota(&self, id: &str) -> Result<KeyQuotaInfo, AiError> {
        self.pool
            .quota_of(id)
            .ok_or_else(|| AiError::KeyNotFound(id.to_string()))
    }

    /// Replaces a key's quota limits (live + persisted). Windows keep their
    /// current usage; only the caps change.
    pub async fn set_key_limits(
        &self,
        id: &str,
        limits: Option<KeyLimits>,
    ) -> Result<(), AiError> {
        if limits.is_some_and(|l| l.max_concurrency.is_some())
            && self.pool.mode() == crate::config::RotationMode::Proactive
        {
            return Err(AiError::InvalidConfig(
                "max_concurrency limits require RotationMode::Reactive".into(),
            ));
        }
        let effective = limits.unwrap_or_else(|| self.pool.default_limits());
        self.pool
            .set_limits(id, effective)
            .ok_or_else(|| AiError::KeyNotFound(id.to_string()))?;
        let (minute, hour, _) = self.pool.windows_of(id).unwrap_or((None, None, effective));
        self.vault
            .store()
            .update_quota(id, limits, minute, hour)
            .await?;
        Ok(())
    }

    /// Removes a key from disk and RAM.
    pub async fn remove_key(&self, id: &str) -> Result<(), AiError> {
        self.vault.store().delete(id).await?;
        self.pool.remove(id);
        Ok(())
    }

    /// Manually bans a key (persisted + live).
    pub async fn ban_key(&self, id: &str) -> Result<(), AiError> {
        let health = KeyHealth::Banned {
            reason: "manually banned".into(),
        };
        self.vault.store().update_health(id, health.clone()).await?;
        self.pool
            .set_health(id, health)
            .ok_or_else(|| AiError::KeyNotFound(id.to_string()))?;
        Ok(())
    }

    /// Restores a banned/cooling key back to `Active` (persisted + live).
    pub async fn recover_key(&self, id: &str) -> Result<(), AiError> {
        self.vault
            .store()
            .update_health(id, KeyHealth::Active)
            .await?;
        self.pool
            .set_health(id, KeyHealth::Active)
            .ok_or_else(|| AiError::KeyNotFound(id.to_string()))?;
        Ok(())
    }

    // ------------------------------------------------------------------
    // Escape hatch
    // ------------------------------------------------------------------

    /// Direct access to the underlying `sqlx::SqlitePool` if the vault is
    /// SQLite-backed. Returns `None` for other backends.
    #[cfg(feature = "sqlite")]
    pub fn raw_sqlite_pool(&self) -> Option<sqlx::SqlitePool> {
        // Downcast through Any is not available on trait objects without
        // extra plumbing; the SqliteStore registers its pool at build time.
        self.pool.raw_sqlite_pool()
    }
}
