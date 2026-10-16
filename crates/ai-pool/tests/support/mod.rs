//! An in-process HTTP/1.1 mock server replaying scripted responses.
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
    /// Sends the body in two writes, split at this byte.
    pub split_at: Option<usize>,
}

impl Scripted {
    pub fn json(status: u16, body: &str) -> Self {
        Self {
            status,
            headers: vec![("content-type".into(), "application/json".into())],
            body: body.to_string(),
            split_at: None,
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

    pub const fn split_at(mut self, at: usize) -> Self {
        self.split_at = Some(at);
        self
    }

    pub fn with_header(mut self, k: &str, v: &str) -> Self {
        self.headers.push((k.into(), v.into()));
        self
    }
}

/// One recorded request.
#[derive(Debug, Clone)]
pub struct Seen {
    pub bearer: String,
    /// The request line, e.g. `GET /v1/models HTTP/1.1`.
    pub line: String,
    /// Every header line, lower-cased names.
    pub headers: Vec<(String, String)>,
    /// The request body, parsed if it is JSON.
    pub body: serde_json::Value,
}

pub struct MockServer {
    pub url: String,
    pub seen: Arc<Mutex<Vec<Seen>>>,
    script: Arc<Mutex<VecDeque<Scripted>>>,
}

impl MockServer {
    /// Replays `responses` in order, then repeats the last one.
    pub async fn start(responses: Vec<Scripted>) -> Self {
        Self::start_with_delay(responses, std::time::Duration::ZERO).await
    }

    /// Like [`Self::start`], sleeping `delay` before each answer.
    pub async fn start_with_delay(responses: Vec<Scripted>, delay: std::time::Duration) -> Self {
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
                    let line = header_str.lines().next().unwrap_or_default().to_string();
                    let headers = header_str
                        .lines()
                        .skip(1)
                        .filter_map(|l| {
                            let (k, v) = l.split_once(':')?;
                            Some((k.trim().to_ascii_lowercase(), v.trim().to_string()))
                        })
                        .collect();
                    let start = (headers_end + 4).min(buf.len());
                    let end = (start + content_length).min(buf.len());
                    let body =
                        serde_json::from_slice(&buf[start..end]).unwrap_or(serde_json::Value::Null);
                    seen.lock().unwrap().push(Seen {
                        bearer,
                        line,
                        headers,
                        body,
                    });

                    let resp = {
                        let mut q = script.lock().unwrap();
                        if q.len() > 1 {
                            q.pop_front().unwrap()
                        } else {
                            q.front()
                                .cloned()
                                .unwrap_or_else(|| Scripted::json(500, "{}"))
                        }
                    };
                    reply(&mut sock, &resp).await;
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

    pub fn requests(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }

    pub fn bearers(&self) -> Vec<String> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .map(|s| s.bearer.clone())
            .collect()
    }

    #[allow(dead_code)]
    pub fn remaining(&self) -> usize {
        self.script.lock().unwrap().len()
    }
}

async fn reply(sock: &mut tokio::net::TcpStream, script: &Scripted) {
    let mut head = format!(
        "HTTP/1.1 {} MOCK\r\ncontent-length: {}\r\nconnection: close\r\n",
        script.status,
        script.body.len()
    );
    for (k, v) in &script.headers {
        let _ = write!(head, "{k}: {v}\r\n");
    }
    head.push_str("\r\n");
    let mut bytes = head.into_bytes();
    let body_start = bytes.len();
    bytes.extend_from_slice(script.body.as_bytes());
    let cut = script.split_at.map_or(bytes.len(), |at| body_start + at);
    let (first, second) = bytes.split_at(cut);
    let _ = sock.write_all(first).await;
    if !second.is_empty() {
        let _ = sock.flush().await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let _ = sock.write_all(second).await;
    }
}

fn find_headers_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}
