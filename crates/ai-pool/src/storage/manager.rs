//! [`KeyManager`]: administration over the vault and the live pool.

use std::sync::Arc;

use secrecy::SecretString;
use serde::{Deserialize, Serialize};

use super::{KeyHealth, KeyStatus, Vault};
use crate::engine::KeyPool;
use crate::error::AiError;
use crate::quota::{KeyLimits, KeyQuotaInfo};

/// Censored key info, safe to show and serialize.
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

    /// Every key, censored, with live health and quota.
    pub async fn list_keys(&self) -> Result<Vec<KeyMetadata>, AiError> {
        let records = self.vault.store().load_all().await?;
        Ok(records
            .into_iter()
            .map(|rec| {
                // Prefer the live in-RAM health (cooldowns tick in real time).
                let health = self.pool.health_of(&rec.id).unwrap_or(rec.health);
                let quota = self.pool.quota_of(&rec.id).filter(|q| {
                    q.minute.is_some()
                        || q.hour.is_some()
                        || q.max_concurrency.is_some()
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

    /// Decrypts one key on demand, e.g. to let the user copy it.
    pub async fn get_decrypted_key(&self, id: &str) -> Result<SecretString, AiError> {
        Ok(self.vault.decrypt_one(id).await?)
    }

    // ------------------------------------------------------------------
    // Key management
    // ------------------------------------------------------------------

    /// Seals, stores and pools a key; returns its id.
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
                "max_concurrency limits require RotationMode::Reactive".into(),
            ));
        }
        // Pasted keys often carry a newline.
        let plaintext_key = plaintext_key.trim();
        if plaintext_key.is_empty() {
            return Err(AiError::InvalidConfig("an API key cannot be empty".into()));
        }
        let id = crate::crypto::deterministic_id(plaintext_key);
        let secret = SecretString::from(plaintext_key.to_string());
        let record = self.vault.insert_plain(&id, &secret, limits).await?;
        self.pool.upsert(
            crate::engine::KeySeed::new(record.id, secret, record.censored)
                .health(record.health)
                .limits(record.limits)
                .windows(record.minute_window, record.hour_window),
        );
        Ok(id)
    }

    /// A live quota snapshot for one key.
    pub fn key_quota(&self, id: &str) -> Result<KeyQuotaInfo, AiError> {
        self.pool
            .quota_of(id)
            .ok_or_else(|| AiError::KeyNotFound(id.to_string()))
    }

    /// Replaces a key's limits live and on disk; windows keep counting.
    pub async fn set_key_limits(&self, id: &str, limits: Option<KeyLimits>) -> Result<(), AiError> {
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
}
