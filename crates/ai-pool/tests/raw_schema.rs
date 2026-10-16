//! Structured output from a plain JSON Schema, with no derive involved.

mod support;

use ai_pool::{AiClient, AiError, ReasoningEffort, ThinkingConfig};
use serde::Deserialize;
use serde_json::json;
use support::{MockServer, Scripted};

#[derive(Debug, Deserialize, PartialEq)]
struct Word {
    word: String,
    note: Option<String>,
}

fn word_schema() -> serde_json::Value {
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "type": "object",
        "properties": {
            "word": { "type": "string", "description": "The headword." },
            "note": { "type": "string" },
            "senses": { "type": "array", "items": {
                "type": "object", "properties": { "gloss": { "type": "string" } }
            }}
        },
        "required": ["word"]
    })
}

async fn client(url: &str) -> AiClient {
    AiClient::builder()
        .default_url(url)
        .api_key("sk-raw-schema-01")
        .build()
        .await
        .unwrap()
}

fn answer(message: &serde_json::Value, finish: &str) -> Scripted {
    let body = json!({
        "id": "c1",
        "choices": [{ "index": 0, "message": message, "finish_reason": finish }]
    });
    Scripted::json(200, &body.to_string())
}

#[tokio::test]
async fn the_payload_is_strict_closed_and_ordered() {
    let c = client("http://127.0.0.1:1/v1").await;
    let payload = c
        .chat_builder()
        .model("m")
        .user("ox")
        .max_completion_tokens(64)
        .thinking(ThinkingConfig::Effort(ReasoningEffort::None))
        .response_json_schema::<Word>("word info", word_schema())
        .into_json();
    assert_eq!(payload["max_completion_tokens"], 64);
    assert_eq!(payload["reasoning_effort"], "none");
    let format = &payload["response_format"]["json_schema"];
    assert_eq!(format["name"], "word_info");
    assert_eq!(format["strict"], true);
    let schema = &format["schema"];
    assert!(schema.get("$schema").is_none());
    assert_eq!(schema["required"], json!(["word", "note", "senses"]));
    assert_eq!(
        schema["properties"]["note"]["type"],
        json!(["string", "null"])
    );
    let item = &schema["properties"]["senses"]["items"];
    assert_eq!(item["additionalProperties"], false);
    let keys: Vec<&String> = schema["properties"].as_object().unwrap().keys().collect();
    assert_eq!(keys, ["word", "note", "senses"]);
}

#[tokio::test]
async fn json_mode_parses_prose_wrapped_answers() {
    let message =
        json!({"role": "assistant", "content": "Here: {\"word\":\"ox\",\"note\":null} done"});
    let server = MockServer::start(vec![answer(&message, "stop")]).await;
    let c = client(&server.url).await;
    let word: Word = c
        .chat_builder()
        .model("m")
        .user("ox")
        .response_json::<Word>()
        .send()
        .await
        .unwrap();
    assert_eq!(
        word,
        Word {
            word: "ox".into(),
            note: None
        }
    );
}

#[tokio::test]
async fn refusals_and_truncation_are_their_own_errors() {
    let refused = json!({"role": "assistant", "content": null, "refusal": "I can't help."});
    let cut = json!({"role": "assistant", "content": "{\"word\":\"o"});
    let server = MockServer::start(vec![answer(&refused, "stop"), answer(&cut, "length")]).await;
    let c = client(&server.url).await;
    let ask = || {
        c.chat_builder()
            .model("m")
            .user("ox")
            .response_json_schema::<Word>("w", word_schema())
    };
    assert!(matches!(ask().send().await, Err(AiError::Refused(r)) if r == "I can't help."));
    assert!(matches!(ask().send().await, Err(AiError::Truncated)));
}
