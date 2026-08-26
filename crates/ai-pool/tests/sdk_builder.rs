//! Tests for `ChatBuilder`: payload construction (thinking config, schemas,
//! sampling) and end-to-end structured-output deserialization.

mod support;

use std::time::Duration;

use ai_pool::{AiClient, JsonSchema, ReasoningEffort, RotationMode, ThinkingConfig};
use serde::Deserialize;
use support::{MockServer, Scripted};

#[derive(Debug, Deserialize, JsonSchema)]
struct SentimentAnalysis {
    /// MUST be exactly "positive", "negative", or "neutral".
    sentiment: String,
    /// A confidence score between 0.0 and 1.0.
    confidence: f64,
}

async fn client(url: &str) -> AiClient {
    AiClient::builder()
        .default_url(url)
        .rotation_mode(RotationMode::Proactive)
        .request_timeout(Duration::from_secs(5))
        .api_keys(vec!["sk-sdk-test-00001"])
        .build()
        .await
        .unwrap()
}

/// Offline client — only used for payload building, never sends.
async fn offline() -> AiClient {
    client("http://127.0.0.1:1/v1").await
}

#[tokio::test]
async fn payload_maps_reasoning_effort() {
    let c = offline().await;
    let payload = c
        .chat_builder()
        .model("o3-mini")
        .user("hi")
        .thinking(ThinkingConfig::Effort(ReasoningEffort::High))
        .into_json();
    assert_eq!(payload["reasoning_effort"], "high");
    assert!(payload.get("thinking").is_none());
}

#[tokio::test]
async fn payload_maps_thinking_budget() {
    let c = offline().await;
    let payload = c
        .chat_builder()
        .model("gemini-2.0-flash-thinking")
        .user("hi")
        .thinking(ThinkingConfig::Enabled {
            budget_tokens: Some(1024),
        })
        .into_json();
    assert_eq!(payload["thinking"]["thinking_budget"], 1024);
    assert!(payload["thinking"].is_object());
}

#[tokio::test]
async fn payload_maps_thinking_disabled_as_null() {
    let c = offline().await;
    let payload = c
        .chat_builder()
        .model("o3-mini")
        .user("hi")
        .thinking(ThinkingConfig::Disabled)
        .into_json();
    assert!(payload["reasoning_effort"].is_null());
    // The key must be PRESENT (explicit null), not absent.
    assert!(payload.as_object().unwrap().contains_key("reasoning_effort"));
}

#[tokio::test]
async fn payload_carries_messages_sampling_and_extras() {
    let c = offline().await;
    let payload = c
        .chat_builder()
        .model("gpt-4o-mini")
        .system("be terse")
        .user("hello")
        .assistant("hi!")
        .temperature(0.2)
        .top_p(0.9)
        .max_tokens(256)
        .extra("seed", serde_json::json!(42))
        .into_json();

    assert_eq!(payload["model"], "gpt-4o-mini");
    let msgs = payload["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 3);
    assert_eq!(msgs[0]["role"], "system");
    assert_eq!(msgs[1]["role"], "user");
    assert_eq!(msgs[2]["role"], "assistant");
    assert!((payload["temperature"].as_f64().unwrap() - 0.2).abs() < f64::EPSILON);
    assert!((payload["top_p"].as_f64().unwrap() - 0.9).abs() < f64::EPSILON);
    assert_eq!(payload["max_tokens"], 256);
    assert_eq!(payload["seed"], 42);
}

#[tokio::test]
async fn payload_injects_json_schema() {
    let c = offline().await;
    let payload = c
        .chat_builder()
        .model("gpt-4o-mini")
        .user("analyze")
        .response_format_schema::<SentimentAnalysis>("sentiment_schema")
        .into_json();

    let rf = &payload["response_format"];
    assert_eq!(rf["type"], "json_schema");
    assert_eq!(rf["json_schema"]["name"], "sentiment_schema");
    assert_eq!(rf["json_schema"]["strict"], true);
    let schema = &rf["json_schema"]["schema"];
    assert_eq!(schema["additionalProperties"], false);
    let props = schema["properties"].as_object().unwrap();
    assert!(props.contains_key("sentiment"));
    assert!(props.contains_key("confidence"));
}

#[tokio::test]
async fn send_deserializes_structured_output() {
    let server = MockServer::start(vec![Scripted::json(
        200,
        r#"{"id":"c1","model":"m","choices":[{"index":0,"message":{"role":"assistant","content":"{\"sentiment\":\"positive\",\"confidence\":0.97}"},"finish_reason":"stop"}]}"#,
    )])
    .await;
    let c = client(&server.url).await;

    let result: SentimentAnalysis = c
        .chat_builder()
        .model("gpt-4o-mini")
        .user("I love this crate!")
        .response_format_schema::<SentimentAnalysis>("sentiment")
        .send()
        .await
        .unwrap();

    assert_eq!(result.sentiment, "positive");
    assert!((result.confidence - 0.97).abs() < f64::EPSILON);
}

#[tokio::test]
async fn send_tolerates_markdown_fenced_json() {
    let server = MockServer::start(vec![Scripted::json(
        200,
        r#"{"id":"c1","model":"m","choices":[{"index":0,"message":{"role":"assistant","content":"```json\n{\"sentiment\":\"negative\",\"confidence\":0.4}\n```"},"finish_reason":"stop"}]}"#,
    )])
    .await;
    let c = client(&server.url).await;

    let result: SentimentAnalysis = c
        .chat_builder()
        .model("gpt-4o-mini")
        .user("meh")
        .response_format_schema::<SentimentAnalysis>("sentiment")
        .send()
        .await
        .unwrap();
    assert_eq!(result.sentiment, "negative");
}

#[tokio::test]
async fn send_text_returns_first_choice() {
    let server = MockServer::start(vec![Scripted::ok_chat("plain text answer")]).await;
    let c = client(&server.url).await;
    let text = c
        .chat_builder()
        .model("gpt-4o-mini")
        .user("hi")
        .send_text()
        .await
        .unwrap();
    assert_eq!(text, "plain text answer");
}

#[cfg(feature = "stream")]
#[tokio::test]
async fn builder_send_stream_sets_stream_flag() {
    use futures_util::StreamExt;

    let sse_body = "data: {\"id\":\"c1\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"streamed\"}}]}\n\ndata: [DONE]\n\n";
    let server = MockServer::start(vec![Scripted {
        status: 200,
        headers: vec![("content-type".into(), "text/event-stream".into())],
        body: sse_body.into(),
    }])
    .await;
    let c = client(&server.url).await;

    let mut stream = c
        .chat_builder()
        .model("gpt-4o-mini")
        .user("go")
        .send_stream()
        .await
        .unwrap();

    let first = stream.next().await.unwrap().unwrap();
    assert_eq!(first.choices[0].delta.content.as_deref(), Some("streamed"));
    assert!(stream.next().await.is_none());
}

#[tokio::test]
async fn doc_comments_become_field_descriptions() {
    // Field-level prompting: /// comments must surface as "description"
    // entries the model reads as per-field instructions.
    let c = offline().await;
    let payload = c
        .chat_builder()
        .model("gpt-4o-mini")
        .user("analyze")
        .response_format_schema::<SentimentAnalysis>("s")
        .into_json();

    let props = &payload["response_format"]["json_schema"]["schema"]["properties"];
    assert!(
        props["sentiment"]["description"]
            .as_str()
            .unwrap()
            .contains("positive"),
    );
    assert!(
        props["confidence"]["description"]
            .as_str()
            .unwrap()
            .contains("0.0 and 1.0"),
    );
}

#[tokio::test]
async fn response_format_text_and_json_variants() {
    use ai_pool::ResponseFormat;

    let c = offline().await;
    let payload = c
        .chat_builder()
        .model("m")
        .user("hi")
        .response_format(ResponseFormat::Text)
        .into_json();
    assert_eq!(payload["response_format"]["type"], "text");

    let payload = c
        .chat_builder()
        .model("m")
        .user("hi")
        .response_format(ResponseFormat::Json)
        .into_json();
    assert_eq!(payload["response_format"]["type"], "json_object");
}
