//! The vault: sealed records in a [`KeyStore`], opened with an envelope.

pub mod memory;
#[cfg(feature = "sqlite")]
pub mod sqlite;

mod manager;
pub use manager::{KeyManager, KeyMetadata};

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use secrecy::SecretString;
use serde::{Deserialize, Serialize};

use crate::crypto::Envelope;
use crate::error::VaultError;
use crate::quota::{HOUR_MS, KeyLimits, MINUTE_MS, WindowState};

/// Health state machine for a single key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyHealth {
    /// Healthy and eligible for selection.
    Active,
    /// Hit a 429; sits out until `retry_after`, then auto-recovers.
    Cooldown {
        /// When the key becomes usable again.
        retry_after: Instant,
    },
    /// Hit a 401/403, failed decryption, or was manually banned.
    Banned {
        /// Why the key was banned.
        reason: String,
    },
}

impl KeyHealth {
    /// Is the key currently usable (Cooldowns expire lazily)?
    pub fn is_usable(&self, now: Instant) -> bool {
        match self {
            Self::Active => true,
            Self::Cooldown { retry_after } => now >= *retry_after,
            Self::Banned { .. } => false,
        }
    }

    /// Persistable form: `(status, reason, cooldown_until_unix_ms)`.
    #[cfg_attr(not(feature = "sqlite"), allow(dead_code))]
    pub(crate) fn to_columns(&self) -> (String, Option<String>, Option<i64>) {
        match self {
            Self::Active => ("active".into(), None, None),
            Self::Cooldown { retry_after } => {
                let remaining = retry_after.saturating_duration_since(Instant::now());
                let until = SystemTime::now() + remaining;
                let ms = i64::try_from(
                    until
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis(),
                )
                .unwrap_or(i64::MAX);
                ("cooldown".into(), None, Some(ms))
            }
            Self::Banned { reason } => ("banned".into(), Some(reason.clone()), None),
        }
    }

    /// Inverse of [`Self::to_columns`]. Expired cooldowns load as `Active`.
    #[cfg_attr(not(feature = "sqlite"), allow(dead_code))]
    pub(crate) fn from_columns(
        status: &str,
        reason: Option<String>,
        cooldown_until_ms: Option<i64>,
    ) -> Self {
        match status {
            "banned" => Self::Banned {
                reason: reason.unwrap_or_else(|| "unknown".into()),
            },
            "cooldown" => {
                let until_ms = u128::try_from(cooldown_until_ms.unwrap_or(0)).unwrap_or(0);
                let now_ms = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis();
                if until_ms > now_ms {
                    let delta = u64::try_from(until_ms - now_ms).unwrap_or(u64::MAX);
                    Self::Cooldown {
                        retry_after: Instant::now() + Duration::from_millis(delta),
                    }
                } else {
                    Self::Active
                }
            }
            _ => Self::Active,
        }
    }

    /// Human-readable status label for UI metadata.
    pub const fn status_label(&self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Cooldown { .. } => "cooldown",
            Self::Banned { .. } => "banned",
        }
    }
}

/// One encrypted key row, as seen by a [`KeyStore`].
#[derive(Debug, Clone)]
pub struct KeyRecord {
    /// Stable id (deterministic hash for seeded keys, random for added ones).
    pub id: String,
    /// `nonce || AES-256-GCM ciphertext+tag` of the plaintext API key.
    pub ciphertext: Vec<u8>,
    /// Display text, so listing keys never decrypts.
    pub censored: String,
    /// Current health.
    pub health: KeyHealth,
    /// Optional per-key quota limits (`None` = use the pool default).
    pub limits: Option<KeyLimits>,
    /// Persisted minute window (reset at load if its time has passed).
    pub minute_window: Option<WindowState>,
    /// Persisted hour window (reset at load if its time has passed).
    pub hour_window: Option<WindowState>,
    /// Unix ms creation timestamp.
    pub created_at_ms: i64,
}

impl KeyRecord {
    /// Drops persisted windows whose time has passed.
    pub(crate) fn expire_stale_windows(&mut self, now_ms: i64) {
        if self
            .minute_window
            .is_some_and(|w| w.expired(now_ms, MINUTE_MS))
        {
            self.minute_window = None;
        }
        if self.hour_window.is_some_and(|w| w.expired(now_ms, HOUR_MS)) {
            self.hour_window = None;
        }
    }
}

/// Storage backend contract. Implementations only ever see encrypted blobs.
#[async_trait]
pub trait KeyStore: Send + Sync {
    /// Loads every record, in stable creation order.
    async fn load_all(&self) -> Result<Vec<KeyRecord>, VaultError>;
    /// Inserts a record; must be idempotent for an existing id.
    async fn insert(&self, record: &KeyRecord) -> Result<(), VaultError>;
    /// Deletes a record by id.
    async fn delete(&self, id: &str) -> Result<(), VaultError>;
    /// Persists a new health state for a record.
    async fn update_health(&self, id: &str, health: KeyHealth) -> Result<(), VaultError>;
    /// Persists quota state (limits + windows) for a record.
    async fn update_quota(
        &self,
        id: &str,
        limits: Option<KeyLimits>,
        minute: Option<WindowState>,
        hour: Option<WindowState>,
    ) -> Result<(), VaultError>;
    /// Fetch a single record (used by explicit decryption).
    async fn get(&self, id: &str) -> Result<KeyRecord, VaultError>;
}

/// A store + the envelope that seals its blobs.
#[derive(Clone)]
pub struct Vault {
    store: std::sync::Arc<dyn KeyStore>,
    envelope: Envelope,
    fresh_master: bool,
}

/// Ban reason of keys sealed by a master key that no longer exists.
pub const LOST_MASTER_KEY: &str = "master key lost; add the key again";

/// A decrypted key ready to be loaded into the Engine.
pub struct DecryptedKey {
    /// Stable key id.
    pub id: String,
    /// Decrypted API key.
    pub secret: SecretString,
    /// Censored display form.
    pub censored: String,
    /// Persisted health state.
    pub health: KeyHealth,
    /// Persisted per-key limits, if any.
    pub limits: Option<KeyLimits>,
    /// Persisted minute window (already expiry-checked).
    pub minute_window: Option<WindowState>,
    /// Persisted hour window (already expiry-checked).
    pub hour_window: Option<WindowState>,
}

impl Vault {
    /// Pairs a storage backend with the envelope that seals its blobs.
    pub fn new(store: std::sync::Arc<dyn KeyStore>, envelope: Envelope) -> Self {
        Self {
            store,
            envelope,
            fresh_master: false,
        }
    }

    /// Marks the master key as just created, so old rows read as lost.
    #[must_use]
    pub const fn fresh_master(mut self, fresh: bool) -> Self {
        self.fresh_master = fresh;
        self
    }

    /// The underlying storage backend.
    pub const fn store(&self) -> &std::sync::Arc<dyn KeyStore> {
        &self.store
    }

    /// Seals `plaintext` under `id`; re-adding heals a row of a lost key.
    pub async fn insert_plain(
        &self,
        id: &str,
        plaintext: &SecretString,
        limits: Option<KeyLimits>,
    ) -> Result<KeyRecord, VaultError> {
        use secrecy::ExposeSecret;
        match self.store.get(id).await {
            Ok(existing) if self.envelope.open(&existing.ciphertext).is_ok() => {
                return Ok(existing);
            }
            Ok(_) => self.store.delete(id).await?,
            Err(VaultError::NotFound(_)) => {}
            Err(e) => return Err(e),
        }
        let record = KeyRecord {
            id: id.to_string(),
            ciphertext: self.envelope.seal(plaintext)?,
            censored: crate::crypto::censor(plaintext.expose_secret()),
            health: KeyHealth::Active,
            limits,
            minute_window: None,
            hour_window: None,
            created_at_ms: monotonic_now_ms(),
        };
        self.store.insert(&record).await?;
        Ok(record)
    }

    /// Decrypts every record; rows that fail are banned and skipped.
    pub async fn load_all_decrypted(&self) -> Result<Vec<DecryptedKey>, VaultError> {
        let records = self.store.load_all().await?;
        let now_ms = crate::quota::now_ms();
        let mut out = Vec::with_capacity(records.len());
        for mut rec in records {
            rec.expire_stale_windows(now_ms);
            match self.envelope.open(&rec.ciphertext) {
                Ok(secret) => out.push(DecryptedKey {
                    id: rec.id,
                    secret,
                    censored: rec.censored,
                    health: rec.health,
                    limits: rec.limits,
                    minute_window: rec.minute_window,
                    hour_window: rec.hour_window,
                }),
                Err(VaultError::Corrupted(why)) => {
                    let reason = if self.fresh_master {
                        LOST_MASTER_KEY.to_string()
                    } else {
                        format!("corrupted: {why}")
                    };
                    tracing::warn!(key_id = %rec.id, %reason, "undecryptable key banned");
                    let banned = KeyHealth::Banned { reason };
                    if let Err(e) = self.store.update_health(&rec.id, banned).await {
                        tracing::warn!(key_id = %rec.id, error = %e, "ban not persisted");
                    }
                }
                Err(e) => return Err(e),
            }
        }
        Ok(out)
    }

    /// Explicitly decrypts a single record (for the "Edit Key" modal).
    pub async fn decrypt_one(&self, id: &str) -> Result<SecretString, VaultError> {
        let rec = self.store.get(id).await?;
        self.envelope.open(&rec.ciphertext)
    }
}

/// Strictly increasing unix milliseconds, so insertion order is stable.
fn monotonic_now_ms() -> i64 {
    use std::sync::atomic::{AtomicI64, Ordering};
    static LAST: AtomicI64 = AtomicI64::new(0);
    let now = i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
    )
    .unwrap_or(i64::MAX);
    LAST.fetch_max(now, Ordering::AcqRel);
    LAST.fetch_add(1, Ordering::AcqRel)
}

/// Serializable health snapshot used inside [`KeyMetadata`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "status", rename_all = "lowercase")]
pub enum KeyStatus {
    /// Healthy and eligible for selection.
    Active,
    /// Rate-limited; waiting out a cooldown.
    Cooldown {
        /// Milliseconds until the key becomes usable again.
        remaining_ms: u64,
    },
    /// Banned until manually recovered.
    Banned {
        /// Why the key was banned.
        reason: String,
    },
}

impl From<&KeyHealth> for KeyStatus {
    fn from(h: &KeyHealth) -> Self {
        match h {
            KeyHealth::Active => Self::Active,
            KeyHealth::Cooldown { retry_after } => Self::Cooldown {
                remaining_ms: u64::try_from(
                    retry_after
                        .saturating_duration_since(Instant::now())
                        .as_millis(),
                )
                .unwrap_or(u64::MAX),
            },
            KeyHealth::Banned { reason } => Self::Banned {
                reason: reason.clone(),
            },
        }
    }
}
