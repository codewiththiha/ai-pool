//! Server-sent events. A mid-stream error ends the stream and cools
//! down or bans its key.

use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use futures_util::stream::Stream;

use crate::error::{AiError, StreamInterruption};
use crate::http::dispatcher::{Call, Dispatcher};
use crate::http::models::{ApiErrorEnvelope, ChatChunk};

/// Parsed chunks; ends after `[DONE]` or the first error.
pub type ChatStream = Pin<Box<dyn Stream<Item = Result<ChatChunk, AiError>> + Send>>;

impl Dispatcher {
    /// Streaming chat completion; forces `"stream": true`.
    pub async fn chat_stream(
        self: &Arc<Self>,
        mut payload: serde_json::Value,
    ) -> Result<ChatStream, AiError> {
        if let Some(obj) = payload.as_object_mut() {
            obj.insert("stream".into(), serde_json::Value::Bool(true));
        }
        let url = self.config.chat_completions_url();
        let (key, resp) = self.execute(&url, Call::Stream(&payload)).await?;

        let parser = SseParser {
            dispatcher: Arc::clone(self),
            key_id: key.as_ref().map(|k| k.id.clone()),
            default_cooldown: self.config.default_cooldown,
        };

        let mut bytes = resp.bytes_stream();
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<ChatChunk, AiError>>(64);

        tokio::spawn(async move {
            // The lease counts against the key's concurrency until the end.
            let _lease = key;
            let mut buf: Vec<u8> = Vec::new();
            'outer: while let Some(frame) = bytes.next().await {
                let chunk = match frame {
                    Ok(chunk) => chunk,
                    Err(e) => {
                        let err = if e.is_timeout() {
                            AiError::Timeout
                        } else {
                            AiError::Network(e)
                        };
                        let _ = tx.send(Err(err)).await;
                        break;
                    }
                };
                buf.extend_from_slice(&chunk);
                // Split on bytes: a character may straddle two frames.
                while let Some(nl) = buf.iter().position(|&b| b == b'\n') {
                    let raw: Vec<u8> = buf.drain(..=nl).collect();
                    let line = String::from_utf8_lossy(&raw);
                    match parser.handle_line(line.trim_end()).await {
                        LineOutcome::Ignore => {}
                        LineOutcome::Done => break 'outer,
                        LineOutcome::Chunk(c) => {
                            if tx.send(Ok(c)).await.is_err() {
                                break 'outer;
                            }
                        }
                        LineOutcome::Fatal(e) => {
                            let _ = tx.send(Err(e)).await;
                            break 'outer;
                        }
                    }
                }
            }
        });

        Ok(Box::pin(futures_util::stream::unfold(rx, |mut rx| async {
            rx.recv().await.map(|item| (item, rx))
        })))
    }
}

enum LineOutcome {
    Ignore,
    Done,
    Chunk(ChatChunk),
    Fatal(AiError),
}

struct SseParser {
    dispatcher: Arc<Dispatcher>,
    key_id: Option<String>,
    default_cooldown: Duration,
}

impl SseParser {
    /// Parses one line; only `data:` lines carry payloads.
    async fn handle_line(&self, line: &str) -> LineOutcome {
        let Some(data) = line.strip_prefix("data:") else {
            return LineOutcome::Ignore;
        };
        let data = data.trim();
        if data.is_empty() {
            return LineOutcome::Ignore;
        }
        if data == "[DONE]" {
            return LineOutcome::Done;
        }

        if let Ok(env) = serde_json::from_str::<ApiErrorEnvelope>(data) {
            let interruption = match env.error.numeric_code() {
                Some(429) => {
                    if let Some(id) = &self.key_id {
                        self.dispatcher
                            .cooldown_key(id, self.default_cooldown)
                            .await;
                    }
                    StreamInterruption::RateLimited
                }
                Some(401 | 403) => {
                    if let Some(id) = &self.key_id {
                        let reason = format!("mid-stream: {}", env.error.message);
                        self.dispatcher.ban_key(id, &reason).await;
                    }
                    StreamInterruption::Unauthorized
                }
                _ => StreamInterruption::Api(env.error.message.clone()),
            };
            return LineOutcome::Fatal(AiError::StreamInterrupted(interruption));
        }

        match serde_json::from_str::<ChatChunk>(data) {
            Ok(chunk) => LineOutcome::Chunk(chunk),
            Err(e) => LineOutcome::Fatal(AiError::InvalidResponse(format!("bad SSE chunk: {e}"))),
        }
    }
}
