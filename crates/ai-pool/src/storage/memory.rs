//! In-memory `KeyStore` backed by a `DashMap`. Perfect for testing or
//! ephemeral sessions: blobs live in RAM and are lost on app exit.

use async_trait::async_trait;
use dashmap::DashMap;

use super::{KeyHealth, KeyRecord, KeyStore};
use crate::error::VaultError;
use crate::quota::{KeyLimits, WindowState};

/// RAM-only `KeyStore`; contents are lost on drop.
#[derive(Default)]
pub struct MemoryStore {
    records: DashMap<String, KeyRecord>,
}

impl MemoryStore {
    /// Creates an empty in-memory store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl KeyStore for MemoryStore {
    async fn load_all(&self) -> Result<Vec<KeyRecord>, VaultError> {
        let mut all: Vec<KeyRecord> = self.records.iter().map(|r| r.value().clone()).collect();
        all.sort_by(|a, b| a.created_at_ms.cmp(&b.created_at_ms).then(a.id.cmp(&b.id)));
        Ok(all)
    }

    async fn insert(&self, record: &KeyRecord) -> Result<(), VaultError> {
        // Idempotent: an existing id keeps its original row (matching the
        // SQLite INSERT OR IGNORE semantics) so re-seeding never clobbers
        // health/quota state.
        self.records
            .entry(record.id.clone())
            .or_insert_with(|| record.clone());
        Ok(())
    }

    async fn delete(&self, id: &str) -> Result<(), VaultError> {
        self.records
            .remove(id)
            .map(|_| ())
            .ok_or_else(|| VaultError::NotFound(id.to_string()))
    }

    async fn update_health(&self, id: &str, health: KeyHealth) -> Result<(), VaultError> {
        match self.records.get_mut(id) {
            Some(mut rec) => {
                rec.health = health;
                Ok(())
            }
            None => Err(VaultError::NotFound(id.to_string())),
        }
    }

    async fn update_quota(
        &self,
        id: &str,
        limits: Option<KeyLimits>,
        minute: Option<WindowState>,
        hour: Option<WindowState>,
    ) -> Result<(), VaultError> {
        match self.records.get_mut(id) {
            Some(mut rec) => {
                rec.limits = limits;
                rec.minute_window = minute;
                rec.hour_window = hour;
                Ok(())
            }
            None => Err(VaultError::NotFound(id.to_string())),
        }
    }

    async fn get(&self, id: &str) -> Result<KeyRecord, VaultError> {
        self.records
            .get(id)
            .map(|r| r.value().clone())
            .ok_or_else(|| VaultError::NotFound(id.to_string()))
    }
}
