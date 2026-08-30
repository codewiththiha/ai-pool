//! SQLx-backed `KeyStore` (feature = "sqlite").
//!
//! Schema (v2 — quota columns added via lightweight migration):
//! ```sql
//! CREATE TABLE ai_pool_keys (
//!     id                TEXT PRIMARY KEY,
//!     ciphertext        BLOB NOT NULL,   -- nonce || AES-256-GCM ct+tag
//!     censored          TEXT NOT NULL,
//!     status            TEXT NOT NULL,   -- active | cooldown | banned
//!     ban_reason        TEXT,
//!     cooldown_until_ms INTEGER,
//!     limits_json       TEXT,            -- serialized KeyLimits, NULL = pool default
//!     minute_start_ms   INTEGER,         -- minute window start (unix ms)
//!     minute_used       INTEGER,
//!     hour_start_ms     INTEGER,         -- hour window start (unix ms)
//!     hour_used         INTEGER,
//!     created_at_ms     INTEGER NOT NULL
//! );
//! ```

use std::path::Path;
use std::str::FromStr;

use async_trait::async_trait;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sqlx::{Row, SqlitePool};

use super::{KeyHealth, KeyRecord, KeyStore};
use crate::error::VaultError;
use crate::quota::{KeyLimits, WindowState};

/// Encrypted-at-rest `KeyStore` persisted in a `SQLite` database.
pub struct SqliteStore {
    pool: SqlitePool,
}

fn db_err(e: &sqlx::Error) -> VaultError {
    VaultError::Storage(e.to_string())
}

impl SqliteStore {
    /// Opens (creating if needed) the database at `path` and runs migrations.
    pub async fn open(path: impl AsRef<Path>) -> Result<Self, VaultError> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| VaultError::Storage(e.to_string()))?;
            }
        }
        let url = format!("sqlite://{}", path.display());
        let opts = SqliteConnectOptions::from_str(&url)
            .map_err(|e| db_err(&e))?
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal);

        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(opts)
            .await
            .map_err(|e| db_err(&e))?;

        sqlx::query(
            r"
            CREATE TABLE IF NOT EXISTS ai_pool_keys (
                id                TEXT PRIMARY KEY,
                ciphertext        BLOB NOT NULL,
                censored          TEXT NOT NULL,
                status            TEXT NOT NULL,
                ban_reason        TEXT,
                cooldown_until_ms INTEGER,
                limits_json       TEXT,
                minute_start_ms   INTEGER,
                minute_used       INTEGER,
                hour_start_ms     INTEGER,
                hour_used         INTEGER,
                created_at_ms     INTEGER NOT NULL
            )
            ",
        )
        .execute(&pool)
        .await
        .map_err(|e| db_err(&e))?;

        // Lightweight migration for databases created before quota support;
        // "duplicate column name" is the expected no-op on current schemas.
        for col in [
            "limits_json TEXT",
            "minute_start_ms INTEGER",
            "minute_used INTEGER",
            "hour_start_ms INTEGER",
            "hour_used INTEGER",
        ] {
            let _ = sqlx::query(&format!("ALTER TABLE ai_pool_keys ADD COLUMN {col}"))
                .execute(&pool)
                .await;
        }

        Ok(Self { pool })
    }

    /// Escape hatch: direct access to the underlying pool.
    pub const fn pool(&self) -> &SqlitePool {
        &self.pool
    }
}

fn window_columns(w: Option<WindowState>) -> (Option<i64>, Option<i64>) {
    w.map_or((None, None), |w| {
        (Some(w.start_ms), Some(i64::from(w.used)))
    })
}

fn window_from_columns(start: Option<i64>, used: Option<i64>) -> Option<WindowState> {
    Some(WindowState {
        start_ms: start?,
        used: u32::try_from(used?).unwrap_or(u32::MAX),
    })
}

fn row_to_record(row: &sqlx::sqlite::SqliteRow) -> KeyRecord {
    let status: String = row.get("status");
    let reason: Option<String> = row.get("ban_reason");
    let cooldown: Option<i64> = row.get("cooldown_until_ms");
    let limits_json: Option<String> = row.get("limits_json");
    KeyRecord {
        id: row.get("id"),
        ciphertext: row.get("ciphertext"),
        censored: row.get("censored"),
        health: KeyHealth::from_columns(&status, reason, cooldown),
        limits: limits_json.and_then(|j| serde_json::from_str(&j).ok()),
        minute_window: window_from_columns(row.get("minute_start_ms"), row.get("minute_used")),
        hour_window: window_from_columns(row.get("hour_start_ms"), row.get("hour_used")),
        created_at_ms: row.get("created_at_ms"),
    }
}

#[async_trait]
impl KeyStore for SqliteStore {
    async fn load_all(&self) -> Result<Vec<KeyRecord>, VaultError> {
        let rows = sqlx::query("SELECT * FROM ai_pool_keys ORDER BY created_at_ms, id")
            .fetch_all(&self.pool)
            .await
            .map_err(|e| db_err(&e))?;
        Ok(rows.iter().map(row_to_record).collect())
    }

    async fn insert(&self, record: &KeyRecord) -> Result<(), VaultError> {
        let (status, reason, cooldown) = record.health.to_columns();
        let limits_json = record
            .limits
            .as_ref()
            .and_then(|l| serde_json::to_string(l).ok());
        let (m_start, m_used) = window_columns(record.minute_window);
        let (h_start, h_used) = window_columns(record.hour_window);
        // Idempotent seeding: INSERT OR IGNORE keeps the original row when the
        // deterministic id already exists (builder re-run after restart).
        sqlx::query(
            r"
            INSERT OR IGNORE INTO ai_pool_keys
              (id, ciphertext, censored, status, ban_reason, cooldown_until_ms,
               limits_json, minute_start_ms, minute_used, hour_start_ms, hour_used,
               created_at_ms)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            ",
        )
        .bind(&record.id)
        .bind(&record.ciphertext)
        .bind(&record.censored)
        .bind(status)
        .bind(reason)
        .bind(cooldown)
        .bind(limits_json)
        .bind(m_start)
        .bind(m_used)
        .bind(h_start)
        .bind(h_used)
        .bind(record.created_at_ms)
        .execute(&self.pool)
        .await
        .map_err(|e| db_err(&e))?;
        Ok(())
    }

    async fn update_quota(
        &self,
        id: &str,
        limits: Option<KeyLimits>,
        minute: Option<WindowState>,
        hour: Option<WindowState>,
    ) -> Result<(), VaultError> {
        let limits_json = limits.as_ref().and_then(|l| serde_json::to_string(l).ok());
        let (m_start, m_used) = window_columns(minute);
        let (h_start, h_used) = window_columns(hour);
        let res = sqlx::query(
            r"
            UPDATE ai_pool_keys
            SET limits_json = ?, minute_start_ms = ?, minute_used = ?,
                hour_start_ms = ?, hour_used = ?
            WHERE id = ?
            ",
        )
        .bind(limits_json)
        .bind(m_start)
        .bind(m_used)
        .bind(h_start)
        .bind(h_used)
        .bind(id)
        .execute(&self.pool)
        .await
        .map_err(|e| db_err(&e))?;
        if res.rows_affected() == 0 {
            return Err(VaultError::NotFound(id.to_string()));
        }
        Ok(())
    }

    async fn delete(&self, id: &str) -> Result<(), VaultError> {
        let res = sqlx::query("DELETE FROM ai_pool_keys WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(|e| db_err(&e))?;
        if res.rows_affected() == 0 {
            return Err(VaultError::NotFound(id.to_string()));
        }
        Ok(())
    }

    async fn update_health(&self, id: &str, health: KeyHealth) -> Result<(), VaultError> {
        let (status, reason, cooldown) = health.to_columns();
        let res = sqlx::query(
            "UPDATE ai_pool_keys SET status = ?, ban_reason = ?, cooldown_until_ms = ? WHERE id = ?",
        )
        .bind(status)
        .bind(reason)
        .bind(cooldown)
        .bind(id)
        .execute(&self.pool)
        .await
        .map_err(|e| db_err(&e))?;
        if res.rows_affected() == 0 {
            return Err(VaultError::NotFound(id.to_string()));
        }
        Ok(())
    }

    async fn get(&self, id: &str) -> Result<KeyRecord, VaultError> {
        let row = sqlx::query("SELECT * FROM ai_pool_keys WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| db_err(&e))?;
        row.map(|r| row_to_record(&r))
            .ok_or_else(|| VaultError::NotFound(id.to_string()))
    }
}
