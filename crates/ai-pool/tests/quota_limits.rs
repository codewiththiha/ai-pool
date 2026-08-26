//! Integration tests for per-key quota intelligence: minute/hour windows,
//! concurrency-aware reactive splitting, realtime introspection endpoints,
//! and the proactive + concurrency configuration guard.

mod support;

use std::time::Duration;

use ai_pool::{AiClient, AiError, KeyLimits, RotationMode};
use support::{MockServer, Scripted};

fn payload() -> serde_json::Value {
    serde_json::json!({"model": "m", "messages": [{"role": "user", "content": "hi"}]})
}

#[tokio::test]
async fn minute_quota_exhausts_then_reports_retry_time() {
    let server = MockServer::start(vec![Scripted::ok_chat("ok")]).await;
    let client = AiClient::builder()
        .default_url(&server.url)
        .rotation_mode(RotationMode::Reactive)
        .request_timeout(Duration::from_secs(5))
        .api_key_with_limits("sk-quota-min-0001", KeyLimits::none().per_minute(3))
        .build()
        .await
        .unwrap();

    for _ in 0..3 {
        client.chat(payload()).await.unwrap();
    }
    // 4th request: quota-blocked before any HTTP call.
    let err = client.chat(payload()).await.unwrap_err();
    match err {
        AiError::AllKeysExhausted { retry_in_ms } => {
            let ms = retry_in_ms.expect("window reset time must be knowable");
            assert!(ms > 0 && ms <= 60_000, "retry_in_ms was {ms}");
        }
        other => panic!("expected AllKeysExhausted, got {other}"),
    }
    // Exactly 3 requests reached the server.
    assert_eq!(server.bearers().len(), 3);
}

#[tokio::test]
async fn quota_info_endpoint_tracks_realtime() {
    let server = MockServer::start(vec![Scripted::ok_chat("ok")]).await;
    let client = AiClient::builder()
        .default_url(&server.url)
        .rotation_mode(RotationMode::Reactive)
        .api_key_with_limits(
            "sk-quota-info-01",
            KeyLimits::none().per_minute(5).per_hour(100),
        )
        .build()
        .await
        .unwrap();

    let id = client.manager().list_keys().await.unwrap()[0].id.clone();

    // Fresh key: minute window unused.
    let q = client.manager().key_quota(&id).unwrap();
    assert_eq!(q.minute.as_ref().unwrap().used, 0);
    assert_eq!(q.minute.as_ref().unwrap().remaining, Some(5));

    client.chat(payload()).await.unwrap();
    client.chat(payload()).await.unwrap();

    let q = client.manager().key_quota(&id).unwrap();
    let minute = q.minute.unwrap();
    assert_eq!(minute.used, 2);
    assert_eq!(minute.remaining, Some(3));
    assert!(minute.resets_in_ms > 0 && minute.resets_in_ms <= 60_000);
    let hour = q.hour.unwrap();
    assert_eq!(hour.used, 2);
    assert_eq!(hour.remaining, Some(98));
    assert!(hour.resets_in_ms > 60_000 && hour.resets_in_ms <= 3_600_000);

    // list_keys carries the same quota snapshot.
    let metas = client.manager().list_keys().await.unwrap();
    assert_eq!(metas[0].quota.as_ref().unwrap().minute.as_ref().unwrap().used, 2);
}

#[tokio::test]
async fn quota_blocked_key_rotates_to_backup() {
    let server = MockServer::start(vec![Scripted::ok_chat("ok")]).await;
    let client = AiClient::builder()
        .default_url(&server.url)
        .rotation_mode(RotationMode::Reactive)
        .api_key_with_limits("sk-limited-00001", KeyLimits::none().per_minute(2))
        .api_key("sk-unlimited-0002")
        .build()
        .await
        .unwrap();

    for _ in 0..6 {
        client.chat(payload()).await.unwrap();
    }
    let bearers = server.bearers();
    let limited = bearers.iter().filter(|b| *b == "sk-limited-00001").count();
    let unlimited = bearers.iter().filter(|b| *b == "sk-unlimited-0002").count();
    assert_eq!(limited, 2, "limited key must stop at its quota");
    assert_eq!(unlimited, 4, "overflow must spill to the unlimited key");
}

#[tokio::test]
async fn proactive_plus_concurrency_is_a_build_error() {
    let err = AiClient::builder()
        .default_url("http://127.0.0.1:1/v1")
        .rotation_mode(RotationMode::Proactive)
        .api_key_with_limits("sk-bad-config-01", KeyLimits::none().max_concurrency(5))
        .build()
        .await
        .unwrap_err();
    assert!(matches!(err, AiError::InvalidConfig(_)), "got: {err}");

    // Also rejected via default limits.
    let err = AiClient::builder()
        .default_url("http://127.0.0.1:1/v1")
        .rotation_mode(RotationMode::Proactive)
        .default_key_limits(KeyLimits::none().max_concurrency(5))
        .api_key("sk-some-key-0001")
        .build()
        .await
        .unwrap_err();
    assert!(matches!(err, AiError::InvalidConfig(_)));

    // And via the manager at runtime.
    let client = AiClient::builder()
        .default_url("http://127.0.0.1:1/v1")
        .rotation_mode(RotationMode::Proactive)
        .api_key("sk-some-key-0001")
        .build()
        .await
        .unwrap();
    let err = client
        .manager()
        .add_key_with_limits("sk-other-key-02", Some(KeyLimits::none().max_concurrency(2)))
        .await
        .unwrap_err();
    assert!(matches!(err, AiError::InvalidConfig(_)));
}

#[tokio::test]
async fn rate_limits_work_fine_with_proactive() {
    let server = MockServer::start(vec![Scripted::ok_chat("ok")]).await;
    let client = AiClient::builder()
        .default_url(&server.url)
        .rotation_mode(RotationMode::Proactive)
        .default_key_limits(KeyLimits::none().per_minute(2))
        .api_keys(vec!["sk-key-aa-000001", "sk-key-bb-000002"])
        .build()
        .await
        .unwrap();

    // 2 per key = 4 total capacity this minute.
    for _ in 0..4 {
        client.chat(payload()).await.unwrap();
    }
    let err = client.chat(payload()).await.unwrap_err();
    assert!(matches!(err, AiError::AllKeysExhausted { retry_in_ms: Some(_) }));
    assert_eq!(server.bearers().len(), 4);
}

#[tokio::test]
async fn concurrency_splits_parallel_requests_across_keys() {
    // Slow server so requests overlap; each key allows 2 concurrent.
    let server = MockServer::start_with_delay(
        vec![Scripted::ok_chat("ok")],
        Duration::from_millis(300),
    )
    .await;
    let client = AiClient::builder()
        .default_url(&server.url)
        .rotation_mode(RotationMode::Reactive)
        .default_key_limits(KeyLimits::none().max_concurrency(2))
        .api_keys(vec!["sk-key-aa-000001", "sk-key-bb-000002"])
        .build()
        .await
        .unwrap();

    // Fire 4 requests at once: without splitting they'd all pile on key A
    // and 2 would fail; with quota intelligence they split 2/2 and all pass.
    let mut handles = Vec::new();
    for _ in 0..4 {
        let c = client.clone();
        handles.push(tokio::spawn(async move { c.chat(payload()).await }));
    }
    for h in handles {
        h.await.unwrap().expect("all 4 must succeed via splitting");
    }
    let bearers = server.bearers();
    let a = bearers.iter().filter(|b| *b == "sk-key-aa-000001").count();
    let b = bearers.iter().filter(|b| *b == "sk-key-bb-000002").count();
    assert_eq!(a, 2, "key A must carry exactly its concurrency cap");
    assert_eq!(b, 2, "overflow must split to key B");
}

#[tokio::test]
async fn saturation_beyond_total_capacity_errors_not_hangs() {
    let server = MockServer::start_with_delay(
        vec![Scripted::ok_chat("ok")],
        Duration::from_millis(400),
    )
    .await;
    let client = AiClient::builder()
        .default_url(&server.url)
        .rotation_mode(RotationMode::Reactive)
        .api_key_with_limits("sk-only-key-0001", KeyLimits::none().max_concurrency(1))
        .build()
        .await
        .unwrap();

    let c1 = client.clone();
    let slow = tokio::spawn(async move { c1.chat(payload()).await });
    tokio::time::sleep(Duration::from_millis(100)).await; // let it lease

    // Second request while the only slot is held: immediate exhaustion.
    let err = client.chat(payload()).await.unwrap_err();
    assert!(matches!(err, AiError::AllKeysExhausted { .. }));

    slow.await.unwrap().unwrap();
    // Slot freed: works again.
    client.chat(payload()).await.unwrap();
}

#[tokio::test]
async fn set_key_limits_updates_live() {
    let server = MockServer::start(vec![Scripted::ok_chat("ok")]).await;
    let client = AiClient::builder()
        .default_url(&server.url)
        .rotation_mode(RotationMode::Reactive)
        .api_key("sk-mutable-00001")
        .build()
        .await
        .unwrap();
    let id = client.manager().list_keys().await.unwrap()[0].id.clone();

    client
        .manager()
        .set_key_limits(&id, Some(KeyLimits::none().per_minute(1)))
        .await
        .unwrap();

    client.chat(payload()).await.unwrap();
    let err = client.chat(payload()).await.unwrap_err();
    assert!(matches!(err, AiError::AllKeysExhausted { retry_in_ms: Some(_) }));

    // Lifting the limit restores service.
    client.manager().set_key_limits(&id, None).await.unwrap();
    client.chat(payload()).await.unwrap();
}
