//! `structured`: modes step down when refused or ignored, and stick.

mod support;

use std::sync::Arc;

use ai_pool::{AiClient, AiError, JsonMode, ModeMemory};
use serde_json::{Value, json};
use support::{MockServer, Scripted};

fn schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "word": { "type": "string", "description": "The headword." },
            "senses": { "type": "array", "items": { "type": "string" } }
        },
        "required": ["word", "senses"]
    })
}

fn says(content: &str) -> Scripted {
    let body = json!({
        "id": "c1",
        "choices": [{
            "index": 0,
            "message": { "role": "assistant", "content": content },
            "finish_reason": "stop"
        }]
    });
    Scripted::json(200, &body.to_string())
}

const GOOD: &str = r#"{"word":"ox","senses":["a bovine"]}"#;
const INVENTED: &str = r#"{"term":"ox","definition":"a bovine"}"#;

async fn client(url: &str, memory: Option<Arc<ModeMemory>>) -> AiClient {
    let builder = AiClient::builder().default_url(url).keyless();
    let builder = match memory {
        Some(m) => builder.json_modes(m),
        None => builder,
    };
    builder.build().await.unwrap()
}

fn format_of(body: &Value) -> Option<&str> {
    body["response_format"]["type"].as_str()
}

#[tokio::test]
async fn an_ignored_schema_steps_down_and_the_mode_sticks() {
    let server = MockServer::start(vec![says(INVENTED), says(GOOD)]).await;
    let c = client(&server.url, None).await;
    let ask = || {
        c.chat_builder()
            .model("gemma")
            .system("Explain words.")
            .user("ox")
            .structured("word", schema())
    };
    let answer = ask().send().await.unwrap();
    assert_eq!(answer.mode, JsonMode::Object);
    assert_eq!(answer.value, serde_json::from_str::<Value>(GOOD).unwrap());
    assert_eq!(answer.mismatch, None);
    let seen = server.requests();
    assert_eq!(format_of(&seen[0].body), Some("json_schema"));
    assert_eq!(format_of(&seen[1].body), Some("json_object"));
    // The schema rides in the system text even when the format carries it.
    let system = seen[0].body["messages"][0]["content"].as_str().unwrap();
    assert!(system.starts_with("Explain words.\n\nReply with one JSON object"));
    assert!(system.contains("\"senses\""));

    assert_eq!(
        c.json_modes().get(&server.url, "gemma"),
        Some(JsonMode::Object)
    );
    ask().send().await.unwrap();
    let seen = server.requests();
    assert_eq!(
        seen.len(),
        3,
        "the next request starts where the last worked"
    );
    assert_eq!(format_of(&seen[2].body), Some("json_object"));
}

#[tokio::test]
async fn a_refused_format_steps_down() {
    let refused = Scripted::json(
        400,
        r#"{"error":{"message":"'response_format.type' must be 'json_schema' or 'text'"}}"#,
    );
    let server = MockServer::start(vec![
        Scripted::json(
            400,
            r#"{"error":{"message":"json_schema is not supported"}}"#,
        ),
        refused,
        says(&format!("```json\n{GOOD}\n```")),
    ])
    .await;
    let c = client(&server.url, None).await;
    let answer = c
        .chat_builder()
        .model("m")
        .system("Explain words.")
        .user("ox")
        .structured("word", schema())
        .send()
        .await
        .unwrap();
    assert_eq!(answer.mode, JsonMode::Prompted);
    let seen = server.requests();
    assert_eq!(seen.len(), 3);
    let last = &seen[2].body;
    assert_eq!(last.get("response_format"), None);
    let messages = last["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 1, "prompted asks in one user message");
    assert_eq!(messages[0]["role"], "user");
    let text = messages[0]["content"].as_str().unwrap();
    assert!(
        text.starts_with("Explain words.\n\nox\n\nAnswer format:"),
        "{text}"
    );
    assert!(text.contains("No markdown, no code fences"));
}

#[tokio::test]
async fn other_failures_do_not_step_down() {
    let server = MockServer::start(vec![Scripted::json(
        400,
        r#"{"error":{"message":"model 'nope' not found"}}"#,
    )])
    .await;
    let c = client(&server.url, None).await;
    let e = c
        .chat_builder()
        .model("nope")
        .user("ox")
        .structured("word", schema())
        .send()
        .await
        .unwrap_err();
    assert!(
        matches!(e, AiError::BadRequest { status: 400, .. }),
        "{e:?}"
    );
    assert_eq!(server.requests().len(), 1);
    assert_eq!(c.json_modes().get(&server.url, "nope"), None);
}

#[tokio::test]
async fn the_last_mode_keeps_what_it_got_and_says_what_is_off() {
    let server = MockServer::start(vec![says(INVENTED)]).await;
    let c = client(&server.url, None).await;
    let answer = c
        .chat_builder()
        .model("m")
        .user("ox")
        .structured("word", schema())
        .send()
        .await
        .unwrap();
    assert_eq!(server.requests().len(), 3);
    assert_eq!(answer.mode, JsonMode::Prompted);
    assert_eq!(answer.value["definition"], "a bovine");
    let why = answer.mismatch.unwrap();
    assert_eq!(why, "$ lacks word, senses; it has term, definition");
}

#[tokio::test]
async fn no_json_at_all_is_an_invalid_response() {
    let server = MockServer::start(vec![says("An ox is a bovine.")]).await;
    let c = client(&server.url, None).await;
    let e = c
        .chat_builder()
        .model("m")
        .user("ox")
        .structured("word", schema())
        .send()
        .await
        .unwrap_err();
    assert!(
        matches!(e, AiError::InvalidResponse(ref m) if m.contains("An ox")),
        "{e:?}"
    );
}

#[tokio::test]
async fn a_shared_memory_is_kept_in_its_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("modes.json");
    let server = MockServer::start(vec![says(INVENTED), says(GOOD)]).await;
    let memory = Arc::new(ModeMemory::open(&path));
    let c = client(&server.url, Some(Arc::clone(&memory))).await;
    let ask = c
        .chat_builder()
        .model("m")
        .user("ox")
        .structured("word", schema());
    assert_eq!(ask.send().await.unwrap().mode, JsonMode::Object);
    // The file is written off the async threads.
    for _ in 0..50 {
        if ModeMemory::open(&path).get(&server.url, "m").is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(
        ModeMemory::open(&path).get(&server.url, "m"),
        Some(JsonMode::Object)
    );
    let fresh = client(&server.url, Some(Arc::new(ModeMemory::open(&path)))).await;
    let pinned = fresh
        .chat_builder()
        .model("m")
        .user("ox")
        .structured("word", schema())
        .mode(JsonMode::Schema);
    assert_eq!(
        format_of(&pinned.payload(JsonMode::Schema)),
        Some("json_schema")
    );
}

#[cfg(feature = "stream")]
mod stream {
    use ai_pool::StructuredEvent;
    use futures_util::StreamExt;

    use super::*;

    fn sse(parts: &[&str]) -> Scripted {
        use std::fmt::Write as _;
        let mut body = String::new();
        for p in parts {
            let chunk = json!({
                "id": "c1",
                "choices": [{ "index": 0, "delta": { "content": p } }]
            });
            let _ = write!(body, "data: {chunk}\n\n");
        }
        body.push_str("data: [DONE]\n\n");
        Scripted {
            status: 200,
            headers: vec![("content-type".into(), "text/event-stream".into())],
            body,
            split_at: None,
        }
    }

    #[tokio::test]
    async fn a_stream_shows_partials_and_restarts_on_a_retry() {
        let server = MockServer::start(vec![
            sse(&[r#"{"term":"ox","#, r#""definition":"a bovine"}"#]),
            sse(&[r#"{"word":"ox","#, r#""senses":["a bo"#, r#"vine"]}"#]),
        ])
        .await;
        let c = client(&server.url, None).await;
        let mut events = c
            .chat_builder()
            .model("m")
            .user("ox")
            .structured("word", schema())
            .send_stream();
        let mut seen = Vec::new();
        while let Some(event) = events.next().await {
            seen.push(event.unwrap());
        }
        assert!(seen.contains(&StructuredEvent::Retry(JsonMode::Object)));
        let retry = seen
            .iter()
            .position(|e| matches!(e, StructuredEvent::Retry(_)))
            .unwrap();
        let after: Vec<&StructuredEvent> = seen.iter().skip(retry + 1).collect();
        assert_eq!(
            after.first(),
            Some(&&StructuredEvent::Partial(json!({"word": "ox"})))
        );
        let Some(StructuredEvent::Done(answer)) = seen.last() else {
            panic!("no answer: {seen:?}");
        };
        assert_eq!(answer.mode, JsonMode::Object);
        assert_eq!(answer.value["senses"], json!(["a bovine"]));
    }
}
