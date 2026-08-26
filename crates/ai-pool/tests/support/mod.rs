//! Minimal in-process HTTP/1.1 mock server (no extra deps) for driving the
//! dispatcher through its semantics table.
//!
//! Compiled independently into each test binary, so not every binary uses
//! every helper.
#![allow(dead_code)]

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// A scripted response the mock server will emit.
#[derive(Clone)]
pub struct Scripted {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl Scripted {
    pub fn json(status: u16, body: &str) -> Self {
        Self {
            status,
            headers: vec![("content-type".into(), "application/json".into())],
            body: body.to_string(),
        }
    }

    #[allow(dead_code)]
    pub fn ok_chat(content: &str) -> Self {
        Self::json(
            200,
            &format!(
                r#"{{"id":"c1","model":"m","choices":[{{"index":0,"message":{{"role":"assistant","content":"{content}"}},"finish_reason":"stop"}}]}}"#
            ),
        )
    }

    pub fn with_header(mut self, k: &str, v: &str) -> Self {
        self.headers.push((k.into(), v.into()));
        self
    }
}

/// One recorded request: (path, authorization bearer token, body).
#[derive(Debug, Clone)]
pub struct Seen {
    pub bearer: String,
}

pub struct MockServer {
    pub url: String,
    pub seen: Arc<Mutex<Vec<Seen>>>,
    script: Arc<Mutex<VecDeque<Scripted>>>,
}

impl MockServer {
    /// Starts the server with a queue of scripted responses. When the queue
    /// runs dry, it keeps replaying the last response.
    pub async fn start(responses: Vec<Scripted>) -> Self {
        Self::start_with_delay(responses, std::time::Duration::ZERO).await
    }

    /// Like [`Self::start`], but sleeps `delay` before answering each
    /// request — lets tests overlap concurrent in-flight requests.
    pub async fn start_with_delay(
        responses: Vec<Scripted>,
        delay: std::time::Duration,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let script: Arc<Mutex<VecDeque<Scripted>>> =
            Arc::new(Mutex::new(responses.into_iter().collect()));
        let seen: Arc<Mutex<Vec<Seen>>> = Arc::new(Mutex::new(Vec::new()));

        let script2 = Arc::clone(&script);
        let seen2 = Arc::clone(&seen);
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let script = Arc::clone(&script2);
                let seen = Arc::clone(&seen2);
                tokio::spawn(async move {
                    if !delay.is_zero() {
                        tokio::time::sleep(delay).await;
                    }
                    let mut buf = Vec::new();
                    let mut tmp = [0u8; 4096];
                    // Read until end of headers, then honor content-length.
                    let (headers_end, header_str) = loop {
                        let n = sock.read(&mut tmp).await.unwrap_or(0);
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(&tmp[..n]);
                        if let Some(pos) = find_headers_end(&buf) {
                            break (pos, String::from_utf8_lossy(&buf[..pos]).to_string());
                        }
                    };
                    let content_length = header_str
                        .lines()
                        .find_map(|l| {
                            let (k, v) = l.split_once(':')?;
                            k.eq_ignore_ascii_case("content-length")
                                .then(|| v.trim().parse::<usize>().ok())?
                        })
                        .unwrap_or(0);
                    while buf.len() < headers_end + 4 + content_length {
                        let n = sock.read(&mut tmp).await.unwrap_or(0);
                        if n == 0 {
                            break;
                        }
                        buf.extend_from_slice(&tmp[..n]);
                    }

                    let bearer = header_str
                        .lines()
                        .find_map(|l| {
                            let (k, v) = l.split_once(':')?;
                            k.eq_ignore_ascii_case("authorization")
                                .then(|| v.trim().trim_start_matches("Bearer ").to_string())
                        })
                        .unwrap_or_default();
                    seen.lock().unwrap().push(Seen { bearer });

                    let resp = {
                        let mut q = script.lock().unwrap();
                        if q.len() > 1 {
                            q.pop_front().unwrap()
                        } else {
                            q.front().cloned().unwrap_or_else(|| Scripted::json(500, "{}"))
                        }
                    };
                    let mut out = format!(
                        "HTTP/1.1 {} MOCK\r\ncontent-length: {}\r\nconnection: close\r\n",
                        resp.status,
                        resp.body.len()
                    );
                    for (k, v) in &resp.headers {
                        let _ = write!(out, "{k}: {v}\r\n");
                    }
                    out.push_str("\r\n");
                    out.push_str(&resp.body);
                    let _ = sock.write_all(out.as_bytes()).await;
                    let _ = sock.shutdown().await;
                });
            }
        });

        Self {
            url: format!("http://{addr}/v1"),
            seen,
            script,
        }
    }

    pub fn bearers(&self) -> Vec<String> {
        self.seen.lock().unwrap().iter().map(|s| s.bearer.clone()).collect()
    }

    #[allow(dead_code)]
    pub fn remaining(&self) -> usize {
        self.script.lock().unwrap().len()
    }
}

fn find_headers_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}
