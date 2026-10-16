//! Error types. Match on variants; messages are for people.

use thiserror::Error;

/// Failures of the key vault: master key, encryption and storage.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum VaultError {
    /// The OS credential store refused access or is not running.
    #[error("OS keychain is locked or unavailable: {0}")]
    KeychainLocked(String),

    /// Any other credential store failure.
    #[error("keychain error: {0}")]
    Keychain(String),

    /// The cipher failed while sealing.
    #[error("crypto failure: {0}")]
    Crypto(String),

    /// A record failed authentication: tampered, or sealed by another key.
    #[error("encrypted record corrupted or tampered with: {0}")]
    Corrupted(String),

    /// The backing store failed.
    #[error("storage error: {0}")]
    Storage(String),

    /// No record has this id.
    #[error("key not found in vault: {0}")]
    NotFound(String),

    /// A master key that is not exactly 32 bytes of hex or raw data.
    #[error("master key must be exactly 32 bytes")]
    InvalidMasterKey,
}

/// Why a stream that had started ended early.
#[cfg(feature = "stream")]
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum StreamInterruption {
    /// The provider reported a rate limit inside the stream.
    RateLimited,
    /// The provider rejected the key inside the stream.
    Unauthorized,
    /// Any other error event.
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

/// Everything a request can fail with.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum AiError {
    /// Every key is cooling down, over quota or banned, or none exist.
    #[error("all keys in the pool are exhausted (rate-limited, quota-blocked, or banned)")]
    AllKeysExhausted {
        /// When the soonest key recovers; `None` when none will by itself.
        retry_in_ms: Option<u64>,
    },

    /// The builder or a key manager call was given contradicting options.
    #[error("invalid configuration: {0}")]
    InvalidConfig(String),

    /// The request outlived `request_timeout`.
    #[error("request timed out")]
    Timeout,

    /// The provider rejected the request itself (400, 404, 413, 422).
    #[error("bad request ({status}): {body}")]
    BadRequest {
        /// HTTP status.
        status: u16,
        /// Response body, truncated.
        body: String,
    },

    /// 5xx responses outlasted the retries.
    #[error("upstream server error ({status}) after retries: {body}")]
    ServerError {
        /// HTTP status.
        status: u16,
        /// Response body, truncated.
        body: String,
    },

    /// A status the retry table does not know.
    #[error("unexpected status ({status}): {body}")]
    UnexpectedStatus {
        /// HTTP status.
        status: u16,
        /// Response body, truncated.
        body: String,
    },

    /// Connecting or transferring failed.
    #[error("network error: {0}")]
    Network(#[from] reqwest::Error),

    /// The body was not what the API promises.
    #[error("invalid response body: {0}")]
    InvalidResponse(String),

    /// The model declined to answer; carries its explanation.
    #[error("the model refused: {0}")]
    Refused(String),

    /// The answer hit the token limit before it was complete.
    #[error("the answer was cut off at the token limit")]
    Truncated,

    /// A started stream ended with an error event.
    #[cfg(feature = "stream")]
    #[error("stream interrupted: {0}")]
    StreamInterrupted(StreamInterruption),

    /// The key vault failed.
    #[error(transparent)]
    Vault(#[from] VaultError),

    /// No key has this id.
    #[error("key not found: {0}")]
    KeyNotFound(String),
}

impl AiError {
    /// True when the same request may succeed later without changes.
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::AllKeysExhausted { retry_in_ms } => retry_in_ms.is_some(),
            Self::Timeout | Self::ServerError { .. } => true,
            Self::Network(e) => e.is_connect() || e.is_timeout(),
            #[cfg(feature = "stream")]
            Self::StreamInterrupted(StreamInterruption::RateLimited) => true,
            _ => false,
        }
    }
}
