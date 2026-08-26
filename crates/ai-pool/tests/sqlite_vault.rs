//! SQLite vault tests (feature = "sqlite"): persistence across restarts,
//! idempotent seeding, and tamper detection via AES-GCM authentication.

#![cfg(feature = "sqlite")]

use ai_pool::{AiClient, ExposeSecret, MasterKeyProvider};

fn master() -> MasterKeyProvider {
    MasterKeyProvider::custom_from_slice(&[42u8; 32]).unwrap()
}

async fn open(db: &std::path::Path, seed: &[&str]) -> AiClient {
    AiClient::builder()
        .default_url("http://127.0.0.1:1/v1") // never dialed in these tests
        .sqlite_store(db)
        .master_key_provider(master())
        .api_keys(seed.to_vec())
        .build()
        .await
        .unwrap()
}

#[tokio::test]
async fn keys_persist_across_restart() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("keys.db");

    {
        let c = open(&db, &["sk-persist-me-0001"]).await;
        assert_eq!(c.manager().list_keys().await.unwrap().len(), 1);
    } // "app exit"

    // "restart" with NO seed keys: the key must load back from disk.
    let c = open(&db, &[]).await;
    let metas = c.manager().list_keys().await.unwrap();
    assert_eq!(metas.len(), 1);

    let secret = c.manager().get_decrypted_key(&metas[0].id).await.unwrap();
    assert_eq!(secret.expose_secret(), "sk-persist-me-0001");
}

#[tokio::test]
async fn seeding_is_idempotent_across_restarts() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("keys.db");

    for _ in 0..3 {
        let c = open(&db, &["sk-seeded-1111", "sk-seeded-2222"]).await;
        assert_eq!(c.manager().list_keys().await.unwrap().len(), 2);
    }
}

#[tokio::test]
async fn health_persists() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("keys.db");

    let id = {
        let c = open(&db, &["sk-tobeban-9999"]).await;
        let id = c.manager().list_keys().await.unwrap()[0].id.clone();
        c.manager().ban_key(&id).await.unwrap();
        id
    };

    let c = open(&db, &[]).await;
    let metas = c.manager().list_keys().await.unwrap();
    assert_eq!(metas[0].id, id);
    assert!(matches!(metas[0].status, ai_pool::KeyStatus::Banned { .. }));

    c.manager().recover_key(&id).await.unwrap();
    let c2 = open(&db, &[]).await;
    assert!(matches!(
        c2.manager().list_keys().await.unwrap()[0].status,
        ai_pool::KeyStatus::Active
    ));
}

#[tokio::test]
async fn tampered_row_is_banned_not_crashing() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("keys.db");

    {
        open(&db, &["sk-tamper-victim-1", "sk-honest-key-2222"]).await;
    }

    // Attacker flips bytes in one encrypted blob directly in SQLite.
    {
        use sqlx::sqlite::SqlitePoolOptions;
        let pool = SqlitePoolOptions::new()
            .connect(&format!("sqlite://{}", db.display()))
            .await
            .unwrap();
        let victim_id = ai_pool::crypto::deterministic_id("sk-tamper-victim-1");
        sqlx::query("UPDATE ai_pool_keys SET ciphertext = X'DEADBEEF' || ciphertext WHERE id = ?")
            .bind(&victim_id)
            .execute(&pool)
            .await
            .unwrap();
        pool.close().await;
    }

    // Restart: app must NOT crash; victim is banned as corrupted, honest key loads.
    let c = open(&db, &[]).await;
    let metas = c.manager().list_keys().await.unwrap();
    assert_eq!(metas.len(), 2);

    let victim_id = ai_pool::crypto::deterministic_id("sk-tamper-victim-1");
    for m in &metas {
        if m.id == victim_id {
            match &m.status {
                ai_pool::KeyStatus::Banned { reason } => {
                    assert!(reason.contains("corrupted"), "reason: {reason}");
                }
                other => panic!("victim should be banned, got {other:?}"),
            }
        } else {
            assert!(matches!(m.status, ai_pool::KeyStatus::Active));
        }
    }
}

#[tokio::test]
async fn wrong_master_key_bans_everything_gracefully() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("keys.db");

    {
        open(&db, &["sk-locked-away-777"]).await;
    }

    // Reopen with a different master key: decryption fails auth, key banned.
    let c = AiClient::builder()
        .default_url("http://127.0.0.1:1/v1")
        .sqlite_store(&db)
        .master_key_provider(MasterKeyProvider::custom_from_slice(&[7u8; 32]).unwrap())
        .build()
        .await
        .unwrap();
    let metas = c.manager().list_keys().await.unwrap();
    assert!(matches!(
        metas[0].status,
        ai_pool::KeyStatus::Banned { .. }
    ));
}

#[tokio::test]
async fn raw_sqlite_pool_escape_hatch() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("keys.db");
    let c = open(&db, &["sk-escape-hatch-1"]).await;

    let pool = c.manager().raw_sqlite_pool().expect("sqlite-backed");
    let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM ai_pool_keys")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count.0, 1);
}

// ---------------------------------------------------------------------------
// Quota persistence
// ---------------------------------------------------------------------------

mod support;

use std::time::Duration;

use ai_pool::{AiError, KeyLimits, RotationMode};
use support::{MockServer, Scripted};

#[tokio::test]
async fn hour_quota_survives_restart_and_sums_up() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("keys.db");
    let server = MockServer::start(vec![Scripted::ok_chat("ok")]).await;
    let payload =
        serde_json::json!({"model": "m", "messages": [{"role": "user", "content": "hi"}]});

    let build = || async {
        AiClient::builder()
            .default_url(&server.url)
            .rotation_mode(RotationMode::Reactive)
            .request_timeout(Duration::from_secs(5))
            .sqlite_store(&db)
            .master_key_provider(master())
            .api_key_with_limits("sk-hourly-000001", KeyLimits::none().per_hour(3))
            .build()
            .await
            .unwrap()
    };

    // Session 1: consume 2 of 3.
    {
        let c = build().await;
        c.chat(payload.clone()).await.unwrap();
        c.chat(payload.clone()).await.unwrap();
        let id = c.manager().list_keys().await.unwrap()[0].id.clone();
        assert_eq!(c.manager().key_quota(&id).unwrap().hour.unwrap().used, 2);
    } // app "exit"

    // Session 2 ("restart"): persisted usage must sum up, not reset.
    let c = build().await;
    let id = c.manager().list_keys().await.unwrap()[0].id.clone();
    let q = c.manager().key_quota(&id).unwrap().hour.unwrap();
    assert_eq!(q.used, 2, "persisted hour usage must reload");
    assert_eq!(q.remaining, Some(1));

    c.chat(payload.clone()).await.unwrap(); // 3rd of 3
    let err = c.chat(payload.clone()).await.unwrap_err();
    assert!(matches!(
        err,
        AiError::AllKeysExhausted {
            retry_in_ms: Some(_)
        }
    ));
    assert_eq!(server.bearers().len(), 3);
}

#[tokio::test]
async fn stale_persisted_window_resets_on_load() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("keys.db");

    {
        open(&db, &["sk-stale-win-0001"]).await;
    }

    // Manually persist an hour window whose time has long passed.
    {
        use sqlx::sqlite::SqlitePoolOptions;
        let pool = SqlitePoolOptions::new()
            .connect(&format!("sqlite://{}", db.display()))
            .await
            .unwrap();
        let id = ai_pool::crypto::deterministic_id("sk-stale-win-0001");
        let limits = serde_json::to_string(&KeyLimits::none().per_hour(5)).unwrap();
        sqlx::query(
            "UPDATE ai_pool_keys SET limits_json = ?, hour_start_ms = 1000, hour_used = 5 \
             WHERE id = ?",
        )
        .bind(limits)
        .bind(&id)
        .execute(&pool)
        .await
        .unwrap();
        pool.close().await;
    }

    // Reload: the expired window must NOT block the key.
    let c = open(&db, &[]).await;
    let id = ai_pool::crypto::deterministic_id("sk-stale-win-0001");
    let q = c.manager().key_quota(&id).unwrap();
    assert_eq!(q.hour.unwrap().used, 0, "stale window must reset to fresh");
}
