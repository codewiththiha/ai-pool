//! Request dispatch and retry policy.
//!
//! Wraps `reqwest`. Takes a plaintext key from the Engine, injects it into
//! the `Authorization: Bearer` header, and fires the request. Implements the
//! hardcoded HTTP semantics table:
//!
//! | HTTP code        | Action                                                  |
//! |------------------|---------------------------------------------------------|
//! | 200              | Return JSON / SSE stream                                |
//! | 429              | Cooldown key (parse `Retry-After`), rotate, retry       |
//! | 401 / 403        | Ban key, rotate, retry                                  |
//! | 400 / 404 / 422  | Fail fast — `AiError::BadRequest`, no rotation          |
//! | 5xx              | Retry SAME key with exponential backoff + jitter        |
//! | timeout          | `AiError::Timeout`, no rotation                         |
//! | pool empty       | `AiError::AllKeysExhausted { retry_in_ms }`             |

use std::sync::Arc;
use std::time::{Duration, Instant};

use secrecy::ExposeSecret;
use tokio::sync::Semaphore;

use crate::config::PoolConfig;
use crate::engine::{KeyPool, LeasedKey};
use crate::error::AiError;
use crate::storage::{KeyHealth, Vault};

use super::models::ChatResponse;

/// Executes requests: injects the bearer token, applies the retry policy,
/// and reports key health back to the pool and vault.
pub struct Dispatcher {
    pub(crate) http: reqwest::Client,
    pub(crate) pool: Arc<KeyPool>,
    pub(crate) vault: Vault,
    pub(crate) config: PoolConfig,
    pub(crate) semaphore: Arc<Semaphore>,
}

/// Outcome of classifying a response status for the retry loop.
pub(crate) enum Classified {
    Ok(reqwest::Response),
    RotateCooldown(Duration),
    RotateBan(String),
    RetrySameKey { status: u16, body: String },
    FailFast(AiError),
}

impl Dispatcher {
    /// Builds the shared `reqwest` client and concurrency semaphore.
    pub fn new(
        pool: Arc<KeyPool>,
        vault: Vault,
        config: PoolConfig,
    ) -> Result<Self, AiError> {
        let http = reqwest::Client::builder()
            .timeout(config.request_timeout)
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

    /// Random 0..500ms jitter so 50 concurrent retries don't stampede.
    pub(crate) fn jitter() -> Duration {
        Duration::from_millis(rand::random::<u64>() % 500)
    }

    /// Parses `Retry-After` (delay-seconds or HTTP-date) with a fallback,
    /// clamped to [`Self::MAX_COOLDOWN`] so a malformed or absurd value can
    /// never park a key for an unreasonable time.
    ///
    /// Handles both wire forms per RFC 7231 §7.1.3: a bare integer number of
    /// seconds, or the `IMF-fixdate` calendar form
    /// (`Wed, 21 Oct 2015 07:28:00 GMT`).
    const MAX_COOLDOWN: Duration = Duration::from_secs(3600);

    pub(crate) fn retry_after(resp: &reqwest::Response, fallback: Duration) -> Duration {
        let header = resp
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .map_or("", str::trim);
        // RFC 7231: either an integer number of seconds, or an absolute
        // IMF-fixdate (`Wed, 21 Oct 2015 07:28:00 GMT`) in the future.
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
                parse_http_date(header).and_then(|at| {
                    (at - now).try_into().ok().map(Duration::from_secs)
                })
            });
        parsed.map_or(fallback, |d| d.min(Self::MAX_COOLDOWN))
    }

    /// Marks a key cooling in RAM and (best-effort) on disk.
    pub(crate) async fn cooldown_key(&self, id: &str, wait: Duration) {
        let health = KeyHealth::Cooldown {
            retry_after: Instant::now() + wait + Self::jitter(),
        };
        self.pool.set_health(id, health.clone());
        if let Err(e) = self.vault.store().update_health(id, health).await {
            tracing::warn!(key_id = %id, error = %e, "failed to persist cooldown");
        }
        tracing::info!(key_id = %id, wait_ms = u64::try_from(wait.as_millis()).unwrap_or(u64::MAX), "key placed in cooldown (429)");
    }

    /// Bans a key in RAM and (best-effort) on disk.
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

    /// One POST attempt with a given key. `stream_accept` switches the
    /// `Accept` header for SSE requests.
    pub(crate) async fn attempt(
        &self,
        key: &LeasedKey,
        url: &str,
        payload: &serde_json::Value,
        sse: bool,
    ) -> Result<reqwest::Response, AiError> {
        let mut req = self
            .http
            .post(url)
            .bearer_auth(key.secret.expose_secret())
            .json(payload);
        if sse {
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

    /// Applies the semantics table to a response.
    pub(crate) async fn classify(&self, resp: reqwest::Response) -> Classified {
        let status = resp.status().as_u16();
        match status {
            200..=299 => Classified::Ok(resp),
            429 => {
                let wait = Self::retry_after(&resp, self.config.default_cooldown);
                Classified::RotateCooldown(wait)
            }
            401 | 403 => {
                let body = resp.text().await.unwrap_or_default();
                Classified::RotateBan(format!("http {status}: {}", truncate(&body, 200)))
            }
            400 | 404 | 422 => {
                let body = resp.text().await.unwrap_or_default();
                Classified::FailFast(AiError::BadRequest { status, body })
            }
            500..=599 => {
                let body = resp.text().await.unwrap_or_default();
                Classified::RetrySameKey { status, body }
            }
            _ => {
                let body = resp.text().await.unwrap_or_default();
                Classified::FailFast(AiError::UnexpectedStatus { status, body })
            }
        }
    }

    /// Full retry loop: rotates keys on 429/401/403, retries the same key on
    /// 5xx with exponential backoff + jitter, fails fast on 4xx client bugs.
    ///
    /// Returns the raw 2xx `reqwest::Response` so callers can either
    /// deserialize JSON or hand the byte stream to the SSE parser.
    pub(crate) async fn execute(
        &self,
        url: &str,
        payload: &serde_json::Value,
        sse: bool,
    ) -> Result<(LeasedKey, reqwest::Response), AiError> {
        let _permit = self
            .semaphore
            .acquire()
            .await
            .expect("semaphore never closed");

        // At most one full pass over the pool per request (each dead key is
        // skipped by the engine automatically on the next lease).
        let max_rotations = self.pool.len().max(1);

        for _ in 0..max_rotations {
            let Some(key) = self.pool.next_key() else {
                return Err(AiError::AllKeysExhausted {
                    retry_in_ms: self.pool.earliest_recovery_ms(),
                });
            };
            // Persist quota windows consumed by this lease (best-effort).
            self.persist_quota(&key.id).await;

            // Inner loop: 5xx retries on the SAME key.
            let mut server_retries = 0u32;
            loop {
                let resp = self.attempt(&key, url, payload, sse).await?;
                match self.classify(resp).await {
                    Classified::Ok(resp) => return Ok((key, resp)),
                    Classified::RotateCooldown(wait) => {
                        self.cooldown_key(&key.id, wait).await;
                        break; // rotate to next key
                    }
                    Classified::RotateBan(reason) => {
                        self.ban_key(&key.id, &reason).await;
                        break; // rotate to next key
                    }
                    Classified::RetrySameKey { status, body } => {
                        if server_retries >= self.config.max_server_error_retries {
                            return Err(AiError::ServerError { status, body });
                        }
                        let backoff =
                            self.config.backoff_base * 2u32.pow(server_retries) + Self::jitter();
                        tracing::debug!(
                            key_id = %key.id, status, retry = server_retries,
                            backoff_ms = u64::try_from(backoff.as_millis()).unwrap_or(u64::MAX),
                            "5xx — retrying same key with backoff"
                        );
                        tokio::time::sleep(backoff).await;
                        server_retries += 1;
                    }
                    Classified::FailFast(err) => return Err(err),
                }
            }
        }
        Err(AiError::AllKeysExhausted {
            retry_in_ms: self.pool.earliest_recovery_ms(),
        })
    }

    /// Best-effort persistence of a key's live quota windows so hour usage
    /// survives restarts. Skipped entirely for keys without rate limits.
    pub(crate) async fn persist_quota(&self, id: &str) {
        let Some((minute, hour, limits)) = self.pool.windows_of(id) else {
            return;
        };
        if !limits.has_rate_limits() {
            return;
        }
        // Persist the per-key limits only if they differ from the pool
        // default (None keeps "inherit default" semantics in storage).
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
    pub async fn chat(&self, payload: serde_json::Value) -> Result<ChatResponse, AiError> {
        let url = self.config.chat_completions_url();
        let (_key, resp) = self.execute(&url, &payload, false).await?;
        let bytes = resp.bytes().await?;
        serde_json::from_slice::<ChatResponse>(&bytes).map_err(|e| {
            AiError::InvalidResponse(format!(
                "{e}; body: {}",
                truncate(&String::from_utf8_lossy(&bytes), 300)
            ))
        })
    }
}

pub(crate) fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let mut end = max;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", &s[..end])
    }
}

/// Parses the RFC 7231 `IMF-fixdate` form of an `HTTP-date`
/// (`Wed, 21 Oct 2015 07:28:00 GMT`) into unix seconds.
///
/// Deliberately dependency-free and case-insensitive on the day/month names,
/// tolerating the leading weekday token (which RFC 7231 says recipients must
/// ignore). Returns `None` on any malformed input.
fn parse_http_date(value: &str) -> Option<i64> {
    const MONTHS: [&str; 12] = [
        "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
    ];
    // "Wed, 21 Oct 2015 07:28:00 GMT"  →  [weekday,] day month year hh:mm:ss
    // Collect non-empty tokens (comma/space separated), then drop the optional
    // leading weekday name and parse day month year hh:mm:ss.
    let mut tokens = value
        .split(|c: char| c.is_whitespace() || c == ',')
        .filter(|t| !t.is_empty());
    // Skip a leading alphabetic weekday token if present ("Wed," / "Wednesday").
    if tokens.clone().next().is_some_and(|t| t.chars().all(|c| c.is_ascii_alphabetic())) {
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

    // Basic gregorian day-of-month validation.
    if day == 0 || day > 31 || hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    // Howard Hinnant's `days_from_civil` (proleptic Gregorian), with a
    // 1-based month (Jan = 1, ... Dec = 12).
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
        assert_eq!(parse_http_date("wednesday, 21 oct 2015 07:28:00 gmt"), Some(ts));
    }

    #[test]
    fn rejects_malformed_dates() {
        assert!(parse_http_date("garbage").is_none());
        assert!(parse_http_date("32 Jan 2015 00:00:00 GMT").is_none());
        assert!(parse_http_date("1 Jan 2015 25:00:00 GMT").is_none());
    }
}
