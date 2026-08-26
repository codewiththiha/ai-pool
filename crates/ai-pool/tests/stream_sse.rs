//! SSE streaming tests (feature = "stream"): happy path, [DONE] handling,
//! and mid-stream 429 error injection.

#![cfg(feature = "stream")]

mod support;

use std::time::Duration;

use ai_pool::{AiClient, AiError, RotationMode, StreamInterruption};
use futures_util::StreamExt;
use support::{MockServer, Scripted};

fn sse(events: &[&str]) -> Scripted {
    use std::fmt::Write as _;
    let mut body = String::new();
    for e in events {
        let _ = write!(body, "data: {e}\n\n");
    }
    Scripted {
        status: 200,
        headers: vec![("content-type".into(), "text/event-stream".into())],
        body,
    }
}

fn chunk(text: &str) -> String {
    format!(
        r#"{{"id":"c1","choices":[{{"index":0,"delta":{{"content":"{text}"}}}}]}}"#
    )
}

async fn client(url: &str, keys: &[&str]) -> AiClient {
    AiClient::builder()
        .default_url(url)
        .rotation_mode(RotationMode::Proactive)
        .request_timeout(Duration::from_secs(5))
        .api_keys(keys.to_vec())
        .build()
        .await
        .unwrap()
}

#[tokio::test]
async fn stream_happy_path_collects_deltas_and_stops_at_done() {
    let server = MockServer::start(vec![sse(&[
        &chunk("Hel"),
        &chunk("lo"),
        &chunk("!"),
        "[DONE]",
    ])])
    .await;
    let c = client(&server.url, &["sk-stream-key-1"]).await;

    let mut stream = c
        .chat_stream(serde_json::json!({"model":"m","messages":[]}))
        .await
        .unwrap();

    let mut text = String::new();
    while let Some(item) = stream.next().await {
        let chunk = item.unwrap();
        if let Some(delta) = chunk.choices.first().and_then(|c| c.delta.content.clone()) {
            text.push_str(&delta);
        }
    }
    assert_eq!(text, "Hello!");
}

#[tokio::test]
async fn mid_stream_rate_limit_aborts_and_cools_key() {
    let server = MockServer::start(vec![sse(&[
        &chunk("partial"),
        r#"{"error":{"code":429,"message":"quota exceeded mid-flight"}}"#,
        &chunk("never-delivered"),
        "[DONE]",
    ])])
    .await;
    let c = client(&server.url, &["sk-stream-key-1"]).await;

    let mut stream = c
        .chat_stream(serde_json::json!({"model":"m","messages":[]}))
        .await
        .unwrap();

    // First chunk arrives fine.
    let first = stream.next().await.unwrap().unwrap();
    assert_eq!(first.choices[0].delta.content.as_deref(), Some("partial"));

    // Then the injected 429 aborts the stream.
    let err = stream.next().await.unwrap().unwrap_err();
    assert!(matches!(
        err,
        AiError::StreamInterrupted(StreamInterruption::RateLimited)
    ));

    // Stream is over — nothing after the error.
    assert!(stream.next().await.is_none());

    // And the key went into cooldown.
    let metas = c.manager().list_keys().await.unwrap();
    assert!(matches!(
        metas[0].status,
        ai_pool::KeyStatus::Cooldown { .. }
    ));
}

#[tokio::test]
async fn mid_stream_auth_error_bans_key() {
    let server = MockServer::start(vec![sse(&[
        r#"{"error":{"code":401,"message":"key revoked"}}"#,
    ])])
    .await;
    let c = client(&server.url, &["sk-stream-key-1"]).await;

    let mut stream = c
        .chat_stream(serde_json::json!({"model":"m","messages":[]}))
        .await
        .unwrap();
    let err = stream.next().await.unwrap().unwrap_err();
    assert!(matches!(
        err,
        AiError::StreamInterrupted(StreamInterruption::Unauthorized)
    ));

    let metas = c.manager().list_keys().await.unwrap();
    assert!(matches!(metas[0].status, ai_pool::KeyStatus::Banned { .. }));
}

#[tokio::test]
async fn pre_stream_429_still_rotates_keys() {
    // 429 on the initial request (before any SSE bytes) must rotate.
    let server = MockServer::start(vec![
        Scripted::json(429, "{}").with_header("retry-after", "60"),
        sse(&[&chunk("ok"), "[DONE]"]),
    ])
    .await;
    let c = client(&server.url, &["sk-key-aaaa-1", "sk-key-bbbb-2"]).await;

    let mut stream = c
        .chat_stream(serde_json::json!({"model":"m","messages":[]}))
        .await
        .unwrap();
    let first = stream.next().await.unwrap().unwrap();
    assert_eq!(first.choices[0].delta.content.as_deref(), Some("ok"));
    assert_eq!(server.bearers().len(), 2);
}
