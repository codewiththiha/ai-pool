//! A [`KeyStore`] in one `SQLite` file, one scope per client, run on
//! tokio's blocking pool.

use std::path::Path;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use rusqlite::types::ValueRef;
use rusqlite::{Connection, OptionalExtension, Row, params};

use super::{KeyHealth, KeyRecord, KeyStore};
use crate::error::VaultError;
use crate::quota::{KeyLimits, WindowState};

const TABLE: &str = "ai_pool_keys_v3";

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS ai_pool_keys_v3 (
    scope             TEXT NOT NULL,
    id                TEXT NOT NULL,
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
    created_at_ms     INTEGER NOT NULL,
    PRIMARY KEY (scope, id)
);";

const COLUMNS: &str = "id, ciphertext, censored, status, ban_reason, cooldown_until_ms, \
     limits_json, minute_start_ms, minute_used, hour_start_ms, hour_used, created_at_ms";

/// Encrypted key rows of one scope inside a SQLite file.
#[derive(Clone)]
pub struct SqliteStore {
    conn: Arc<Mutex<Connection>>,
    scope: Arc<str>,
}

impl std::fmt::Debug for SqliteStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SqliteStore")
            .field("scope", &self.scope)
            .finish_non_exhaustive()
    }
}

fn db_err(e: &rusqlite::Error) -> VaultError {
    VaultError::Storage(e.to_string())
}

impl SqliteStore {
    /// Opens or creates the file at `path` and uses rows of `scope`.
    pub async fn open(path: impl AsRef<Path>, scope: &str) -> Result<Self, VaultError> {
        let path = path.as_ref().to_path_buf();
        let conn = tokio::task::spawn_blocking(move || Self::connect(&path))
            .await
            .map_err(|e| VaultError::Storage(e.to_string()))??;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            scope: Arc::from(scope),
        })
    }

    fn connect(path: &Path) -> Result<Connection, VaultError> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent).map_err(|e| VaultError::Storage(e.to_string()))?;
        }
        let conn = Connection::open(path).map_err(|e| db_err(&e))?;
        conn.busy_timeout(std::time::Duration::from_secs(5))
            .map_err(|e| db_err(&e))?;
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(|e| db_err(&e))?;
        conn.execute_batch(SCHEMA).map_err(|e| db_err(&e))?;
        migrate_v2(&conn)?;
        Ok(conn)
    }

    /// Runs `job` with the connection on the blocking pool.
    async fn with<T, F>(&self, job: F) -> Result<T, VaultError>
    where
        T: Send + 'static,
        F: FnOnce(&Connection, &str) -> Result<T, VaultError> + Send + 'static,
    {
        let conn = Arc::clone(&self.conn);
        let scope = Arc::clone(&self.scope);
        tokio::task::spawn_blocking(move || {
            let guard = conn
                .lock()
                .map_err(|_| VaultError::Storage("connection lock poisoned".into()))?;
            job(&guard, &scope)
        })
        .await
        .map_err(|e| VaultError::Storage(e.to_string()))?
    }
}

/// Copies rows of the unscoped 0.2 table into the empty scope once.
fn migrate_v2(conn: &Connection) -> Result<(), VaultError> {
    let old: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='ai_pool_keys')",
            [],
            |row| row.get(0),
        )
        .map_err(|e| db_err(&e))?;
    if old {
        conn.execute_batch(&format!(
            "INSERT OR IGNORE INTO {TABLE} (scope, {COLUMNS}) SELECT '', {COLUMNS} FROM ai_pool_keys;
             ALTER TABLE ai_pool_keys RENAME TO ai_pool_keys_v2_migrated;"
        ))
        .map_err(|e| db_err(&e))?;
    }
    Ok(())
}

fn window_columns(w: Option<WindowState>) -> (Option<i64>, Option<i64>) {
    w.map_or((None, None), |w| {
        (Some(w.start_ms), Some(i64::from(w.used)))
    })
}

fn window_from_columns(start: Option<i64>, used: Option<i64>) -> Option<WindowState> {
    Some(WindowState {
        start_ms: start?,
        used: u32::try_from(used?.max(0)).unwrap_or(u32::MAX),
    })
}

fn limits_json(limits: Option<&KeyLimits>) -> Option<String> {
    limits.and_then(|l| serde_json::to_string(l).ok())
}

/// Any column type as bytes; a mistyped blob then fails to open.
fn bytes_of(value: ValueRef<'_>) -> Vec<u8> {
    match value {
        ValueRef::Blob(b) | ValueRef::Text(b) => b.to_vec(),
        _ => Vec::new(),
    }
}

fn row_to_record(row: &Row<'_>) -> rusqlite::Result<KeyRecord> {
    let status: String = row.get("status")?;
    let limits: Option<String> = row.get("limits_json")?;
    Ok(KeyRecord {
        id: row.get("id")?,
        ciphertext: bytes_of(row.get_ref("ciphertext")?),
        censored: row.get("censored")?,
        health: KeyHealth::from_columns(
            &status,
            row.get("ban_reason")?,
            row.get("cooldown_until_ms")?,
        ),
        limits: limits.and_then(|j| serde_json::from_str(&j).ok()),
        minute_window: window_from_columns(row.get("minute_start_ms")?, row.get("minute_used")?),
        hour_window: window_from_columns(row.get("hour_start_ms")?, row.get("hour_used")?),
        created_at_ms: row.get("created_at_ms")?,
    })
}

fn require_row(changed: usize, id: &str) -> Result<(), VaultError> {
    if changed == 0 {
        Err(VaultError::NotFound(id.to_string()))
    } else {
        Ok(())
    }
}

#[async_trait]
impl KeyStore for SqliteStore {
    async fn load_all(&self) -> Result<Vec<KeyRecord>, VaultError> {
        self.with(|conn, scope| {
            let sql = format!(
                "SELECT {COLUMNS} FROM {TABLE} WHERE scope = ?1 ORDER BY created_at_ms, id"
            );
            let mut stmt = conn.prepare(&sql).map_err(|e| db_err(&e))?;
            let rows = stmt
                .query_map([scope], row_to_record)
                .map_err(|e| db_err(&e))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
                .map_err(|e| db_err(&e))
        })
        .await
    }

    async fn insert(&self, record: &KeyRecord) -> Result<(), VaultError> {
        let record = record.clone();
        self.with(move |conn, scope| {
            let (status, reason, cooldown) = record.health.to_columns();
            let (m_start, m_used) = window_columns(record.minute_window);
            let (h_start, h_used) = window_columns(record.hour_window);
            let sql = format!(
                "INSERT OR IGNORE INTO {TABLE} (scope, {COLUMNS})
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)"
            );
            conn.execute(
                &sql,
                params![
                    scope,
                    record.id,
                    record.ciphertext,
                    record.censored,
                    status,
                    reason,
                    cooldown,
                    limits_json(record.limits.as_ref()),
                    m_start,
                    m_used,
                    h_start,
                    h_used,
                    record.created_at_ms,
                ],
            )
            .map_err(|e| db_err(&e))?;
            Ok(())
        })
        .await
    }

    async fn delete(&self, id: &str) -> Result<(), VaultError> {
        let id = id.to_string();
        self.with(move |conn, scope| {
            let sql = format!("DELETE FROM {TABLE} WHERE scope = ?1 AND id = ?2");
            let changed = conn.execute(&sql, [scope, &id]).map_err(|e| db_err(&e))?;
            require_row(changed, &id)
        })
        .await
    }

    async fn update_health(&self, id: &str, health: KeyHealth) -> Result<(), VaultError> {
        let id = id.to_string();
        self.with(move |conn, scope| {
            let (status, reason, cooldown) = health.to_columns();
            let sql = format!(
                "UPDATE {TABLE} SET status = ?1, ban_reason = ?2, cooldown_until_ms = ?3
                 WHERE scope = ?4 AND id = ?5"
            );
            let changed = conn
                .execute(&sql, params![status, reason, cooldown, scope, id])
                .map_err(|e| db_err(&e))?;
            require_row(changed, &id)
        })
        .await
    }

    async fn update_quota(
        &self,
        id: &str,
        limits: Option<KeyLimits>,
        minute: Option<WindowState>,
        hour: Option<WindowState>,
    ) -> Result<(), VaultError> {
        let id = id.to_string();
        self.with(move |conn, scope| {
            let (m_start, m_used) = window_columns(minute);
            let (h_start, h_used) = window_columns(hour);
            let sql = format!(
                "UPDATE {TABLE} SET limits_json = ?1, minute_start_ms = ?2, minute_used = ?3,
                 hour_start_ms = ?4, hour_used = ?5 WHERE scope = ?6 AND id = ?7"
            );
            let changed = conn
                .execute(
                    &sql,
                    params![
                        limits_json(limits.as_ref()),
                        m_start,
                        m_used,
                        h_start,
                        h_used,
                        scope,
                        id
                    ],
                )
                .map_err(|e| db_err(&e))?;
            require_row(changed, &id)
        })
        .await
    }

    async fn get(&self, id: &str) -> Result<KeyRecord, VaultError> {
        let id = id.to_string();
        self.with(move |conn, scope| {
            let sql = format!("SELECT {COLUMNS} FROM {TABLE} WHERE scope = ?1 AND id = ?2");
            conn.query_row(&sql, [scope, &id], row_to_record)
                .optional()
                .map_err(|e| db_err(&e))?
                .ok_or(VaultError::NotFound(id))
        })
        .await
    }
}
