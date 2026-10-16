//! The HTTP retry table, driven through the whole client.

mod support;

use std::time::Duration;

use ai_pool::{AiClient, AiError, ExposeSecret, RotationMode};
use support::{MockServer, Scripted};

fn chat_payload() -> serde_json::Value {
    serde_json::json!({
        "model": "test-model",
        "messages": [{"role": "user", "content": "hi"}]
    })
}

async fn client(url: &str, keys: &[&str], mode: RotationMode) -> AiClient {
    AiClient::builder()
        .default_url(url)
        .rotation_mode(mode)
        .request_timeout(Duration::from_secs(5))
        .max_server_error_retries(2)
        .api_keys(keys.to_vec())
        .build()
        .await
        .unwrap()
}

#[tokio::test]
async fn ok_returns_parsed_json() {
    let server = MockServer::start(vec![Scripted::ok_chat("hello world")]).await;
    let c = client(&server.url, &["sk-aaaa-1111"], RotationMode::Proactive).await;
    let resp = c.chat(chat_payload()).await.unwrap();
    assert_eq!(resp.choices[0].message.content, "hello world");
    assert_eq!(server.bearers(), vec!["sk-aaaa-1111"]);
}

#[tokio::test]
async fn rate_limit_rotates_to_next_key_and_retries() {
    // First key hits 429, second key succeeds.
    let server = MockServer::start(vec![
        Scripted::json(429, r#"{"error":{"message":"slow down"}}"#)
            .with_header("retry-after", "60"),
        Scripted::ok_chat("recovered"),
    ])
    .await;
    let c = client(
        &server.url,
        &["sk-key-one-11", "sk-key-two-22"],
        RotationMode::Proactive,
    )
    .await;

    let resp = c.chat(chat_payload()).await.unwrap();
    assert_eq!(resp.choices[0].message.content, "recovered");

    let bearers = server.bearers();
    assert_eq!(bearers.len(), 2);
    assert_ne!(bearers[0], bearers[1], "must have rotated keys");

    // The 429'd key must now be in cooldown.
    let metas = c.manager().list_keys().await.unwrap();
    let status_of = |m: &ai_pool::KeyMetadata| -> String {
        serde_json::to_value(&m.status).unwrap()["status"]
            .as_str()
            .unwrap()
            .into()
    };
    assert!(metas.iter().any(|m| status_of(m) == "cooldown"));
    assert!(metas.iter().any(|m| status_of(m) == "active"));
}

#[tokio::test]
async fn unauthorized_bans_key_and_rotates() {
    let server = MockServer::start(vec![
        Scripted::json(401, r#"{"error":{"message":"bad key"}}"#),
        Scripted::ok_chat("second key worked"),
    ])
    .await;
    let c = client(
        &server.url,
        &["sk-badkey-0000", "sk-goodkey-1111"],
        RotationMode::Reactive,
    )
    .await;

    let resp = c.chat(chat_payload()).await.unwrap();
    assert_eq!(resp.choices[0].message.content, "second key worked");

    let metas = c.manager().list_keys().await.unwrap();
    let banned = metas
        .iter()
        .filter(|m| matches!(m.status, ai_pool::KeyStatus::Banned { .. }))
        .count();
    assert_eq!(banned, 1);
}

#[tokio::test]
async fn bad_request_fails_fast_without_rotation() {
    let server = MockServer::start(vec![Scripted::json(
        400,
        r#"{"error":{"message":"model not found"}}"#,
    )])
    .await;
    let c = client(
        &server.url,
        &["sk-key-one-11", "sk-key-two-22"],
        RotationMode::Proactive,
    )
    .await;

    let err = c.chat(chat_payload()).await.unwrap_err();
    assert!(matches!(err, AiError::BadRequest { status: 400, .. }));
    // Exactly ONE request: no rotation happened.
    assert_eq!(server.bearers().len(), 1);

    // Both keys must still be active.
    let metas = c.manager().list_keys().await.unwrap();
    assert!(
        metas
            .iter()
            .all(|m| matches!(m.status, ai_pool::KeyStatus::Active))
    );
}

#[tokio::test]
async fn server_error_retries_same_key_then_gives_up() {
    let server = MockServer::start(vec![
        Scripted::json(503, "oops"),
        Scripted::json(503, "oops"),
        Scripted::json(503, "oops"),
    ])
    .await;
    let c = client(&server.url, &["sk-only-key-99"], RotationMode::Proactive).await;

    let err = c.chat(chat_payload()).await.unwrap_err();
    assert!(matches!(err, AiError::ServerError { status: 503, .. }));

    // 1 initial + 2 retries, all on the SAME key.
    let bearers = server.bearers();
    assert_eq!(bearers.len(), 3);
    assert!(bearers.iter().all(|b| b == "sk-only-key-99"));
}

#[tokio::test]
async fn server_error_recovers_mid_retry() {
    let server = MockServer::start(vec![
        Scripted::json(502, "bad gateway"),
        Scripted::ok_chat("finally"),
    ])
    .await;
    let c = client(&server.url, &["sk-only-key-99"], RotationMode::Proactive).await;
    let resp = c.chat(chat_payload()).await.unwrap();
    assert_eq!(resp.choices[0].message.content, "finally");
}

#[tokio::test]
async fn all_keys_exhausted() {
    let server = MockServer::start(vec![
        Scripted::json(429, "{}").with_header("retry-after", "120"),
    ])
    .await;
    let c = client(
        &server.url,
        &["sk-key-one-11", "sk-key-two-22"],
        RotationMode::Proactive,
    )
    .await;

    let err = c.chat(chat_payload()).await.unwrap_err();
    assert!(matches!(err, AiError::AllKeysExhausted { .. }));
    // Both keys were tried once each before exhaustion.
    assert_eq!(server.bearers().len(), 2);
}

#[tokio::test]
async fn empty_pool_is_exhausted_immediately() {
    let server = MockServer::start(vec![Scripted::ok_chat("unreachable")]).await;
    let c = client(&server.url, &[], RotationMode::Proactive).await;
    let err = c.chat(chat_payload()).await.unwrap_err();
    assert!(matches!(err, AiError::AllKeysExhausted { .. }));
    assert_eq!(server.bearers().len(), 0);
}

#[tokio::test]
async fn proactive_spreads_requests_round_robin() {
    let server = MockServer::start(vec![Scripted::ok_chat("ok")]).await;
    let c = client(
        &server.url,
        &["sk-key-one-11", "sk-key-two-22"],
        RotationMode::Proactive,
    )
    .await;
    for _ in 0..4 {
        c.chat(chat_payload()).await.unwrap();
    }
    let bearers = server.bearers();
    assert_eq!(bearers.len(), 4);
    assert_ne!(bearers[0], bearers[1]);
    assert_eq!(bearers[0], bearers[2]);
    assert_eq!(bearers[1], bearers[3]);
}

#[tokio::test]
async fn manager_crud_and_censoring() {
    let server = MockServer::start(vec![Scripted::ok_chat("ok")]).await;
    let c = client(&server.url, &[], RotationMode::Proactive).await;
    let m = c.manager();

    let id = m.add_key("sk-proj-abcdefgh8f92").await.unwrap();
    let metas = m.list_keys().await.unwrap();
    assert_eq!(metas.len(), 1);
    assert_eq!(metas[0].censored_key, "sk-proj...8f92");
    assert!(!metas[0].censored_key.contains("abcdefgh"));

    // Explicit decryption round-trips the plaintext.
    let secret = m.get_decrypted_key(&id).await.unwrap();
    assert_eq!(secret.expose_secret(), "sk-proj-abcdefgh8f92");

    // Ban / recover
    m.ban_key(&id).await.unwrap();
    assert!(matches!(
        m.list_keys().await.unwrap()[0].status,
        ai_pool::KeyStatus::Banned { .. }
    ));
    m.recover_key(&id).await.unwrap();
    assert!(matches!(
        m.list_keys().await.unwrap()[0].status,
        ai_pool::KeyStatus::Active
    ));

    // Remove
    m.remove_key(&id).await.unwrap();
    assert!(m.list_keys().await.unwrap().is_empty());
}

#[tokio::test]
async fn idempotent_seeding_no_duplicates() {
    let server = MockServer::start(vec![Scripted::ok_chat("ok")]).await;
    // Same key passed twice + re-adding via manager must not duplicate.
    let c = client(
        &server.url,
        &["sk-same-key-123", "sk-same-key-123"],
        RotationMode::Proactive,
    )
    .await;
    assert_eq!(c.manager().list_keys().await.unwrap().len(), 1);
    c.manager().add_key("sk-same-key-123").await.unwrap();
    assert_eq!(c.manager().list_keys().await.unwrap().len(), 1);
}

async fn status_of(c: &AiClient) -> Vec<ai_pool::KeyStatus> {
    let metas = c.manager().list_keys().await.unwrap();
    metas.into_iter().map(|m| m.status).collect()
}

#[tokio::test]
async fn keys_that_cannot_work_are_banned_and_rotated_past() {
    let bodies = [
        (402, r#"{"error":{"message":"Insufficient credits"}}"#),
        (
            400,
            r#"{"error":{"message":"API key not valid. Please pass a valid API key.","status":"INVALID_ARGUMENT"}}"#,
        ),
        (
            429,
            r#"{"error":{"message":"You exceeded your current quota","code":"insufficient_quota"}}"#,
        ),
    ];
    for (status, body) in bodies {
        let server =
            MockServer::start(vec![Scripted::json(status, body), Scripted::ok_chat("ok")]).await;
        let c = client(
            &server.url,
            &["sk-dead-key-01", "sk-live-key-02"],
            RotationMode::Reactive,
        )
        .await;
        let resp = c.chat(chat_payload()).await.unwrap();
        assert_eq!(resp.choices[0].message.content, "ok", "status {status}");
        let statuses = status_of(&c).await;
        assert!(
            statuses
                .iter()
                .any(|s| matches!(s, ai_pool::KeyStatus::Banned { .. })),
            "status {status} should ban: {statuses:?}"
        );
    }
}

#[tokio::test]
async fn an_ordinary_bad_request_still_fails_fast() {
    let server = MockServer::start(vec![Scripted::json(
        400,
        r#"{"error":{"message":"Unknown parameter: foo"}}"#,
    )])
    .await;
    let c = client(
        &server.url,
        &["sk-fine-key-001", "sk-fine-key-002"],
        RotationMode::Proactive,
    )
    .await;
    assert!(matches!(
        c.chat(chat_payload()).await,
        Err(AiError::BadRequest { status: 400, .. })
    ));
    assert_eq!(server.requests().len(), 1);
}

#[tokio::test]
async fn request_timeouts_are_retried_on_the_same_key() {
    let server = MockServer::start(vec![
        Scripted::json(408, "{}"),
        Scripted::ok_chat("second time lucky"),
    ])
    .await;
    let c = client(&server.url, &["sk-only-key-01"], RotationMode::Proactive).await;
    let resp = c.chat(chat_payload()).await.unwrap();
    assert_eq!(resp.choices[0].message.content, "second time lucky");
    assert_eq!(server.bearers(), vec!["sk-only-key-01", "sk-only-key-01"]);
}

#[tokio::test]
async fn keyless_clients_send_no_authorization() {
    let server = MockServer::start(vec![Scripted::ok_chat("local")]).await;
    let c = AiClient::builder()
        .default_url(&server.url)
        .keyless()
        .header("X-Title", "mareader")
        .build()
        .await
        .unwrap();
    let resp = c.chat(chat_payload()).await.unwrap();
    assert_eq!(resp.choices[0].message.content, "local");
    let seen = &server.requests()[0];
    assert!(seen.headers.iter().all(|(k, _)| k != "authorization"));
    assert!(
        seen.headers
            .contains(&("x-title".into(), "mareader".into()))
    );
}

#[tokio::test]
async fn a_keyless_rate_limit_reports_when_to_retry() {
    let server = MockServer::start(vec![
        Scripted::json(429, "{}").with_header("retry-after", "7"),
    ])
    .await;
    let c = AiClient::builder()
        .default_url(&server.url)
        .keyless()
        .build()
        .await
        .unwrap();
    match c.chat(chat_payload()).await {
        Err(
            e @ AiError::AllKeysExhausted {
                retry_in_ms: Some(7000),
            },
        ) => {
            assert!(e.is_retryable());
        }
        other => panic!("expected exhaustion with a wait, got {other:?}"),
    }
}

#[tokio::test]
async fn a_named_auth_header_carries_the_key() {
    let server = MockServer::start(vec![Scripted::ok_chat("ok")]).await;
    let c = AiClient::builder()
        .default_url(&server.url)
        .auth(ai_pool::Auth::Header("x-goog-api-key".into()))
        .api_key("AIza-header-key-1")
        .build()
        .await
        .unwrap();
    c.chat(chat_payload()).await.unwrap();
    let seen = &server.requests()[0];
    assert!(
        seen.headers
            .contains(&("x-goog-api-key".into(), "AIza-header-key-1".into()))
    );
    assert!(seen.headers.iter().all(|(k, _)| k != "authorization"));
}

#[tokio::test]
async fn an_invalid_header_is_a_config_error() {
    let built = AiClient::builder().header("bad header", "x").build().await;
    assert!(matches!(built, Err(AiError::InvalidConfig(_))));
}

#[tokio::test]
async fn models_are_listed_sorted_without_prefixes() {
    let server = MockServer::start(vec![Scripted::json(
        200,
        r#"{"object":"list","data":[{"id":"models/gemini-2.5-pro"},{"id":"gemini-2.5-flash","owned_by":"google"}]}"#,
    )])
    .await;
    let c = client(&server.url, &["sk-list-models-1"], RotationMode::Proactive).await;
    let ids: Vec<String> = c
        .models()
        .await
        .unwrap()
        .into_iter()
        .map(|m| m.id)
        .collect();
    assert_eq!(ids, ["gemini-2.5-flash", "gemini-2.5-pro"]);
    assert!(server.requests()[0].line.starts_with("GET /v1/models "));
}

#[tokio::test]
async fn pasted_keys_are_trimmed_and_blank_ones_refused() {
    let server = MockServer::start(vec![Scripted::ok_chat("ok")]).await;
    let c = client(
        &server.url,
        &["  ", "sk-padded-key-1\n"],
        RotationMode::Proactive,
    )
    .await;
    assert_eq!(c.manager().list_keys().await.unwrap().len(), 1);
    assert!(matches!(
        c.manager().add_key(" \n").await,
        Err(AiError::InvalidConfig(_))
    ));
    c.chat(chat_payload()).await.unwrap();
    assert_eq!(server.bearers(), vec!["sk-padded-key-1"]);
}
