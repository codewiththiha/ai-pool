//! SSE parsing (feature = "stream"): `data:` framing, `[DONE]` handling, and
//! mid-stream error detection.
//!
//! Edge case handled: the API returns 200 OK, starts streaming, then injects
//! `{"error": {"code": 429, ...}}` *inside* the stream. The parser catches
//! it, aborts the stream, marks the key `Cooldown`/`Banned` accordingly, and
//! yields `AiError::StreamInterrupted(...)`.

use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use futures_util::stream::Stream;
use futures_util::StreamExt;

use crate::error::{AiError, StreamInterruption};
use crate::http::dispatcher::Dispatcher;
use crate::http::models::{ApiErrorEnvelope, ChatChunk};

/// A stream of parsed completion chunks. Ends after `[DONE]` or the first
/// fatal error.
pub type ChatStream = Pin<Box<dyn Stream<Item = Result<ChatChunk, AiError>> + Send>>;

impl Dispatcher {
    /// Streaming chat completion. Forces `"stream": true` into the payload
    /// and parses the SSE response into [`ChatChunk`]s.
    pub async fn chat_stream(
        self: &Arc<Self>,
        mut payload: serde_json::Value,
    ) -> Result<ChatStream, AiError> {
        if let Some(obj) = payload.as_object_mut() {
            obj.insert("stream".into(), serde_json::Value::Bool(true));
        }
        let url = self.config.chat_completions_url();
        let (key, resp) = self.execute(&url, &payload, true).await?;

        let parser = SseParser {
            dispatcher: Arc::clone(self),
            key_id: key.id.clone(),
            default_cooldown: self.config.default_cooldown,
        };

        let mut bytes = resp.bytes_stream();
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<ChatChunk, AiError>>(64);

        tokio::spawn(async move {
            // Hold the lease for the whole stream: the request stays
            // in-flight against the key's concurrency quota until the SSE
            // connection finishes or the consumer drops the stream.
            let _lease = key;
            let mut buf = String::new();
            'outer: while let Some(frame) = bytes.next().await {
                match frame {
                    Err(e) => {
                        let err = if e.is_timeout() {
                            AiError::Timeout
                        } else {
                            AiError::Network(e)
                        };
                        let _ = tx.send(Err(err)).await;
                        break 'outer;
                    }
                    Ok(chunk) => {
                        buf.push_str(&String::from_utf8_lossy(&chunk));
                        // Process every complete line; keep the partial tail.
                        while let Some(nl) = buf.find('\n') {
                            let line: String = buf.drain(..=nl).collect();
                            match parser.handle_line(line.trim_end()).await {
                                LineOutcome::Ignore => {}
                                LineOutcome::Done => break 'outer,
                                LineOutcome::Chunk(c) => {
                                    if tx.send(Ok(c)).await.is_err() {
                                        break 'outer; // receiver dropped
                                    }
                                }
                                LineOutcome::Fatal(e) => {
                                    let _ = tx.send(Err(e)).await;
                                    break 'outer;
                                }
                            }
                        }
                    }
                }
            }
            // tx drops here -> stream ends.
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
    key_id: String,
    default_cooldown: Duration,
}

impl SseParser {
    /// Parses one SSE line. Only `data:` lines carry payloads; comments
    /// (`:`), `event:` lines, and blank keep-alives are ignored.
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

        // Mid-stream error injection (the 200-then-429 trap).
        if let Ok(env) = serde_json::from_str::<ApiErrorEnvelope>(data) {
            let interruption = match env.error.numeric_code() {
                Some(429) => {
                    self.dispatcher
                        .cooldown_key(&self.key_id, self.default_cooldown)
                        .await;
                    StreamInterruption::RateLimited
                }
                Some(401 | 403) => {
                    self.dispatcher
                        .ban_key(&self.key_id, &format!("mid-stream: {}", env.error.message))
                        .await;
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
