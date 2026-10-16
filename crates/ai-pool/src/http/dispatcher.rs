//! The retry loop: leases keys, sends requests, classifies answers.

use std::sync::Arc;
use std::time::{Duration, Instant};

use secrecy::ExposeSecret;
use serde_json::Value;
use tokio::sync::Semaphore;

use crate::config::{Auth, PoolConfig};
use crate::engine::{KeyPool, LeasedKey};
use crate::error::AiError;
use crate::storage::{KeyHealth, Vault};

use super::models::{ChatResponse, ModelInfo, ModelList};

/// Sends requests through the pool. Shared by every clone of a client.
pub struct Dispatcher {
    pub(crate) http: reqwest::Client,
    pub(crate) pool: Arc<KeyPool>,
    pub(crate) vault: Vault,
    pub(crate) config: PoolConfig,
    pub(crate) semaphore: Arc<Semaphore>,
}

/// One HTTP exchange the loop can repeat.
#[derive(Clone, Copy)]
pub(crate) enum Call<'a> {
    /// A JSON POST answered with JSON.
    Post(&'a Value),
    /// A JSON POST answered with server-sent events.
    #[cfg_attr(not(feature = "stream"), allow(dead_code))]
    Stream(&'a Value),
    /// A plain GET.
    Get,
}

/// What a response means for the loop.
pub(crate) enum Classified {
    Ok(reqwest::Response),
    RotateCooldown(Duration),
    RotateBan(String),
    RetrySameKey { status: u16, body: String },
    FailFast(AiError),
}

/// How one key's attempts ended.
enum Sent {
    Answer(Option<LeasedKey>, reqwest::Response),
    Rotate,
}

/// Body fragments providers use for a key that is wrong, not throttled.
const KEY_REJECTED: [&str; 5] = [
    "api_key_invalid",
    "api key not valid",
    "invalid api key",
    "invalid_api_key",
    "incorrect api key",
];

/// Body fragments of a 429 that no waiting will fix.
const QUOTA_GONE: [&str; 2] = ["insufficient_quota", "exceeded your current quota"];

fn mentions(body: &str, needles: &[&str]) -> bool {
    let body = body.to_ascii_lowercase();
    needles.iter().any(|n| body.contains(n))
}

impl Dispatcher {
    /// Builds the HTTP client; fails on a header that is not valid HTTP.
    pub fn new(pool: Arc<KeyPool>, vault: Vault, config: PoolConfig) -> Result<Self, AiError> {
        let mut headers = reqwest::header::HeaderMap::new();
        for (name, value) in &config.headers {
            let name = reqwest::header::HeaderName::from_bytes(name.as_bytes())
                .map_err(|e| AiError::InvalidConfig(format!("header {name}: {e}")))?;
            let value = reqwest::header::HeaderValue::from_str(value)
                .map_err(|e| AiError::InvalidConfig(format!("header {name}: {e}")))?;
            headers.insert(name, value);
        }
        if let Auth::Header(name) = &config.auth {
            reqwest::header::HeaderName::from_bytes(name.as_bytes())
                .map_err(|e| AiError::InvalidConfig(format!("auth header {name}: {e}")))?;
        }
        let http = reqwest::Client::builder()
            .timeout(config.request_timeout)
            .connect_timeout(config.connect_timeout)
            .default_headers(headers)
            .build()?;
        let semaphore = Arc::new(Semaphore::new(config.concurrency_limit));
        Ok(Self {
            http,
            pool,
            vault,
            config,
            semaphore,
        })
    }

    pub(crate) fn jitter() -> Duration {
        Duration::from_millis(rand::random::<u64>() % 500)
    }

    const MAX_COOLDOWN: Duration = Duration::from_secs(3600);

    /// `Retry-After` as seconds or an HTTP date, capped at an hour.
    pub(crate) fn retry_after(resp: &reqwest::Response, fallback: Duration) -> Duration {
        let header = resp
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .map_or("", str::trim);
        let parsed = header
            .parse::<u64>()
            .ok()
            .map(Duration::from_secs)
            .or_else(|| {
                let now = i64::try_from(
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs(),
                )
                .unwrap_or(i64::MIN);
                parse_http_date(header)
                    .and_then(|at| at.checked_sub(now))
                    .and_then(|secs| u64::try_from(secs).ok())
                    .map(Duration::from_secs)
            });
        parsed.map_or(fallback, |d| d.min(Self::MAX_COOLDOWN))
    }

    /// Cools a key down in RAM and, best effort, on disk.
    pub(crate) async fn cooldown_key(&self, id: &str, wait: Duration) {
        let health = KeyHealth::Cooldown {
            retry_after: Instant::now() + wait + Self::jitter(),
        };
        self.pool.set_health(id, health.clone());
        if let Err(e) = self.vault.store().update_health(id, health).await {
            tracing::warn!(key_id = %id, error = %e, "failed to persist cooldown");
        }
        let wait_ms = u64::try_from(wait.as_millis()).unwrap_or(u64::MAX);
        tracing::info!(key_id = %id, wait_ms, "key cooling down");
    }

    /// Bans a key in RAM and, best effort, on disk.
    pub(crate) async fn ban_key(&self, id: &str, reason: &str) {
        let health = KeyHealth::Banned {
            reason: reason.to_string(),
        };
        self.pool.set_health(id, health.clone());
        if let Err(e) = self.vault.store().update_health(id, health).await {
            tracing::warn!(key_id = %id, error = %e, "failed to persist ban");
        }
        tracing::warn!(key_id = %id, %reason, "key banned");
    }

    /// One attempt, with `key` when the pool has keys.
    pub(crate) async fn attempt(
        &self,
        key: Option<&LeasedKey>,
        url: &str,
        call: Call<'_>,
    ) -> Result<reqwest::Response, AiError> {
        let mut req = match call {
            Call::Post(body) | Call::Stream(body) => self.http.post(url).json(body),
            Call::Get => self.http.get(url),
        };
        if let Some(key) = key {
            let secret = key.secret.expose_secret();
            req = match &self.config.auth {
                Auth::Bearer => req.bearer_auth(secret),
                Auth::Header(name) => req.header(name.as_str(), secret),
                Auth::None => req,
            };
        }
        if matches!(call, Call::Stream(_)) {
            req = req.header(reqwest::header::ACCEPT, "text/event-stream");
        }
        req.send().await.map_err(|e| {
            if e.is_timeout() {
                AiError::Timeout
            } else {
                AiError::Network(e)
            }
        })
    }

    /// Maps a response onto the retry table.
    pub(crate) async fn classify(&self, resp: reqwest::Response) -> Classified {
        let status = resp.status().as_u16();
        if resp.status().is_success() {
            return Classified::Ok(resp);
        }
        let wait = Self::retry_after(&resp, self.config.default_cooldown);
        let body = resp.text().await.unwrap_or_default();
        let shown = truncate(&body, 300);
        match status {
            429 if mentions(&body, &QUOTA_GONE) => {
                Classified::RotateBan(format!("quota exhausted: {}", truncate(&body, 200)))
            }
            429 => Classified::RotateCooldown(wait),
            401 | 403 => Classified::RotateBan(format!("http {status}: {}", truncate(&body, 200))),
            402 => Classified::RotateBan("payment required".into()),
            400 if mentions(&body, &KEY_REJECTED) => {
                Classified::RotateBan(format!("http 400: {}", truncate(&body, 200)))
            }
            400 | 404 | 413 | 422 => Classified::FailFast(AiError::BadRequest {
                status,
                body: shown,
            }),
            408 | 500..=599 => Classified::RetrySameKey {
                status,
                body: shown,
            },
            _ => Classified::FailFast(AiError::UnexpectedStatus {
                status,
                body: shown,
            }),
        }
    }

    /// Sends `call`, rotating past dead keys; returns the lease too.
    pub(crate) async fn execute(
        &self,
        url: &str,
        call: Call<'_>,
    ) -> Result<(Option<LeasedKey>, reqwest::Response), AiError> {
        let _permit = self
            .semaphore
            .acquire()
            .await
            .map_err(|_| AiError::InvalidConfig("client is shutting down".into()))?;
        if self.config.auth == Auth::None {
            return match self.send_with(None, url, call).await? {
                Sent::Answer(key, resp) => Ok((key, resp)),
                Sent::Rotate => Err(AiError::InvalidConfig("keyless rotation".into())),
            };
        }
        for _ in 0..self.pool.len().max(1) {
            let Some(key) = self.pool.next_key() else {
                break;
            };
            self.persist_quota(&key.id).await;
            if let Sent::Answer(key, resp) = self.send_with(Some(key), url, call).await? {
                return Ok((key, resp));
            }
        }
        Err(AiError::AllKeysExhausted {
            retry_in_ms: self.pool.earliest_recovery_ms(),
        })
    }

    /// Tries one key, retrying server errors on it with backoff.
    async fn send_with(
        &self,
        key: Option<LeasedKey>,
        url: &str,
        call: Call<'_>,
    ) -> Result<Sent, AiError> {
        let mut retries = 0u32;
        loop {
            let resp = self.attempt(key.as_ref(), url, call).await?;
            let classified = self.classify(resp).await;
            match (classified, key.as_ref()) {
                (Classified::Ok(resp), _) => return Ok(Sent::Answer(key, resp)),
                (Classified::FailFast(err), _) => return Err(err),
                (Classified::RetrySameKey { status, body }, _) => {
                    if retries >= self.config.max_server_error_retries {
                        return Err(AiError::ServerError { status, body });
                    }
                    let step = 2u32.checked_pow(retries).unwrap_or(u32::MAX);
                    let backoff = self.config.backoff_base.saturating_mul(step) + Self::jitter();
                    tracing::debug!(status, retries, "server error; retrying the same key");
                    tokio::time::sleep(backoff).await;
                    retries += 1;
                }
                (Classified::RotateCooldown(wait), Some(key)) => {
                    self.cooldown_key(&key.id, wait).await;
                    return Ok(Sent::Rotate);
                }
                (Classified::RotateBan(reason), Some(key)) => {
                    self.ban_key(&key.id, &reason).await;
                    return Ok(Sent::Rotate);
                }
                (Classified::RotateCooldown(wait), None) => {
                    return Err(AiError::AllKeysExhausted {
                        retry_in_ms: u64::try_from(wait.as_millis()).ok(),
                    });
                }
                (Classified::RotateBan(reason), None) => {
                    return Err(AiError::UnexpectedStatus {
                        status: 401,
                        body: reason,
                    });
                }
            }
        }
    }

    /// Persists a key's quota windows so hourly use survives restarts.
    pub(crate) async fn persist_quota(&self, id: &str) {
        let Some((minute, hour, limits)) = self.pool.windows_of(id) else {
            return;
        };
        if !limits.has_rate_limits() {
            return;
        }
        // `None` keeps "inherit the pool default" in storage.
        let stored_limits = (limits != self.pool.default_limits()).then_some(limits);
        if let Err(e) = self
            .vault
            .store()
            .update_quota(id, stored_limits, minute, hour)
            .await
        {
            tracing::debug!(key_id = %id, error = %e, "failed to persist quota windows");
        }
    }

    /// Non-streaming chat completion.
    pub async fn chat(&self, payload: Value) -> Result<ChatResponse, AiError> {
        let url = self.config.chat_completions_url();
        let (_key, resp) = self.execute(&url, Call::Post(&payload)).await?;
        let bytes = resp.bytes().await?;
        serde_json::from_slice::<ChatResponse>(&bytes).map_err(|e| {
            AiError::InvalidResponse(format!(
                "{e}; body: {}",
                truncate(&String::from_utf8_lossy(&bytes), 300)
            ))
        })
    }

    /// The models the endpoint offers, sorted by id.
    pub async fn models(&self) -> Result<Vec<ModelInfo>, AiError> {
        let url = self.config.models_url();
        let (_key, resp) = self.execute(&url, Call::Get).await?;
        let bytes = resp.bytes().await?;
        let list = serde_json::from_slice::<ModelList>(&bytes).map_err(|e| {
            AiError::InvalidResponse(format!(
                "{e}; body: {}",
                truncate(&String::from_utf8_lossy(&bytes), 300)
            ))
        })?;
        let mut models = list.into_models();
        models.sort_by(|a, b| a.id.cmp(&b.id));
        models.dedup_by(|a, b| a.id == b.id);
        Ok(models)
    }
}

/// `s` cut to at most `max` bytes on a character boundary.
pub(crate) fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let end = (0..=max)
        .rev()
        .find(|&i| s.is_char_boundary(i))
        .unwrap_or(0);
    format!("{}…", s.get(..end).unwrap_or_default())
}

/// Unix seconds of an RFC 7231 date: `Wed, 21 Oct 2015 07:28:00 GMT`.
fn parse_http_date(value: &str) -> Option<i64> {
    const MONTHS: [&str; 12] = [
        "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
    ];
    let mut tokens = value
        .split(|c: char| c.is_whitespace() || c == ',')
        .filter(|t| !t.is_empty());
    // Recipients must ignore the weekday.
    if tokens
        .clone()
        .next()
        .is_some_and(|t| t.chars().all(|c| c.is_ascii_alphabetic()))
    {
        tokens.next();
    }
    let day: i64 = tokens.next()?.parse().ok()?;
    let month_str = tokens.next()?.to_ascii_lowercase();
    let month_n: i64 = i64::try_from(MONTHS.iter().position(|&m| m == month_str)?).ok()?;
    let year: i64 = tokens.next()?.parse().ok()?;
    let time = tokens.next()?;
    let mut hm = time.split(':');
    let hour: i64 = hm.next()?.parse().ok()?;
    let minute: i64 = hm.next()?.parse().ok()?;
    let second: i64 = hm.next()?.parse().ok()?;

    if day == 0 || day > 31 || hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    // Howard Hinnant's `days_from_civil`, months counted from 1.
    let m = month_n + 1;
    let y = if m <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + day - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + hour * 3600 + minute * 60 + second)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_imf_fixdate() {
        // "Wed, 21 Oct 2015 07:28:00 GMT" == 1445412480 unix.
        let ts = parse_http_date("Wed, 21 Oct 2015 07:28:00 GMT").unwrap();
        assert_eq!(ts, 1_445_412_480_i64);
        // Leading weekday is ignored; case-insensitive.
        assert_eq!(
            parse_http_date("wednesday, 21 oct 2015 07:28:00 gmt"),
            Some(ts)
        );
    }

    #[test]
    fn rejects_malformed_dates() {
        assert!(parse_http_date("garbage").is_none());
        assert!(parse_http_date("32 Jan 2015 00:00:00 GMT").is_none());
        assert!(parse_http_date("1 Jan 2015 25:00:00 GMT").is_none());
    }
}
