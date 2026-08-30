//! `thiserror` definitions for the two error domains:
//! - [`VaultError`]: storage and cryptography failures
//! - [`AiError`]: everything callers see from the public API

use thiserror::Error;

/// Errors originating in the Vault layer (storage backends + envelope encryption).
#[derive(Debug, Error)]
pub enum VaultError {
    /// The OS keychain is locked or unavailable (a common state on macOS
    /// after sleep). Callers should prompt the user to unlock and retry.
    #[error("OS keychain is locked or unavailable")]
    KeychainLocked,

    /// Any other keychain failure.
    #[error("keychain error: {0}")]
    Keychain(String),

    /// AES-GCM encryption/decryption failure that is *not* a tamper event.
    #[error("crypto failure: {0}")]
    Crypto(String),

    /// AES-256-GCM authentication failed: the encrypted blob was modified on
    /// disk (database tampering) or the master key changed. The offending key
    /// is marked `Banned("corrupted")` instead of crashing the app.
    #[error("encrypted record corrupted or tampered with: {0}")]
    Corrupted(String),

    /// Storage backend failure (`SQLite` I/O, pool errors, ...).
    #[error("storage error: {0}")]
    Storage(String),

    /// No record with this id exists in the store.
    #[error("key not found in vault: {0}")]
    NotFound(String),

    /// A custom master key was provided that is not exactly 32 bytes.
    #[error("master key must be exactly 32 bytes")]
    InvalidMasterKey,
}

/// Reason a stream was aborted mid-flight.
#[cfg(feature = "stream")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamInterruption {
    /// The provider injected a 429-style error object inside the SSE stream.
    RateLimited,
    /// The provider injected a 401/403-style error object inside the SSE stream.
    Unauthorized,
    /// Any other in-stream API error.
    Api(String),
}

#[cfg(feature = "stream")]
impl std::fmt::Display for StreamInterruption {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RateLimited => write!(f, "rate limited mid-stream"),
            Self::Unauthorized => write!(f, "unauthorized mid-stream"),
            Self::Api(msg) => write!(f, "api error mid-stream: {msg}"),
        }
    }
}

/// Errors returned by [`crate::AiClient`] and [`crate::KeyManager`].
#[derive(Debug, Error)]
pub enum AiError {
    /// Every key in the pool is currently unusable (cooldown, ban, exhausted
    /// rate window, or concurrency saturation).
    ///
    /// `retry_in_ms` is the soonest known recovery time: `Some(ms)` when a
    /// cooldown or rate window is the blocker, `None` when recovery time is
    /// unknowable (all banned, or saturated without a configured cooldown).
    #[error("all keys in the pool are exhausted (rate-limited, quota-blocked, or banned)")]
    AllKeysExhausted {
        /// Milliseconds until the soonest key frees up, if knowable.
        retry_in_ms: Option<u64>,
    },

    /// The builder was configured invalidly (e.g. per-key `max_concurrency`
    /// combined with `RotationMode::Proactive`).
    #[error("invalid configuration: {0}")]
    InvalidConfig(String),

    /// The request timed out. The key is NOT rotated: the network is at fault.
    #[error("request timed out")]
    Timeout,

    /// 400 / 404 / 422 — the request itself is wrong. Fail fast, never rotate.
    #[error("bad request ({status}): {body}")]
    BadRequest {
        /// HTTP status code.
        status: u16,
        /// Raw response body.
        body: String,
    },

    /// 5xx persisted after exponential-backoff retries on the same key.
    #[error("upstream server error ({status}) after retries: {body}")]
    ServerError {
        /// HTTP status code.
        status: u16,
        /// Raw response body.
        body: String,
    },

    /// A status code outside the hardcoded semantics table.
    #[error("unexpected status ({status}): {body}")]
    UnexpectedStatus {
        /// HTTP status code.
        status: u16,
        /// Raw response body.
        body: String,
    },

    /// Transport-level failure (DNS, TLS, connection reset, ...).
    #[error("network error: {0}")]
    Network(#[from] reqwest::Error),

    /// 200 OK but the body could not be deserialized.
    #[error("invalid response body: {0}")]
    InvalidResponse(String),

    /// The SSE stream returned 200 OK, started streaming, then injected an
    /// error object. The stream is aborted and the key's health updated.
    #[cfg(feature = "stream")]
    #[error("stream interrupted: {0}")]
    StreamInterrupted(StreamInterruption),

    /// Bubbled-up vault failure.
    #[error(transparent)]
    Vault(#[from] VaultError),

    /// A `KeyManager` operation referenced an unknown key id.
    #[error("key not found: {0}")]
    KeyNotFound(String),
}
