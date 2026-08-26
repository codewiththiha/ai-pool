//! Timeouts, limits, default URLs and rotation configuration.

use std::time::Duration;

use crate::quota::KeyLimits;

/// How the Engine picks the next key from the pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RotationMode {
    /// Atomic round-robin cursor: every request gets the next key in the pool
    /// (`index % pool_size`). The right default when requests arrive
    /// concurrently and you want load spread evenly.
    #[default]
    Proactive,
    /// Stick to one "primary" key until it fails (429/401), then fall back to
    /// the next healthy key.
    Reactive,
}

/// Resolved runtime configuration shared by the Engine and the Dispatcher.
#[derive(Debug, Clone)]
pub struct PoolConfig {
    /// Base URL of an OpenAI-compatible API, e.g.
    /// `https://api.openai.com/v1/` or
    /// `https://generativelanguage.googleapis.com/v1beta/openai/`.
    pub default_url: String,
    /// Rotation strategy.
    pub rotation_mode: RotationMode,
    /// Maximum concurrent in-flight requests (enforced with a semaphore).
    pub concurrency_limit: usize,
    /// Per-request timeout.
    pub request_timeout: Duration,
    /// How many times a 5xx is retried on the *same* key with exponential
    /// backoff before giving up.
    pub max_server_error_retries: u32,
    /// Cooldown applied on a 429 when the server does not send `Retry-After`.
    pub default_cooldown: Duration,
    /// Base delay for 5xx exponential backoff (doubled per attempt + jitter).
    pub backoff_base: Duration,
    /// Default quota limits applied to keys without per-key limits.
    pub default_limits: KeyLimits,
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            default_url: "https://api.openai.com/v1/".to_string(),
            rotation_mode: RotationMode::Proactive,
            concurrency_limit: 32,
            request_timeout: Duration::from_secs(60),
            max_server_error_retries: 3,
            default_cooldown: Duration::from_secs(30),
            backoff_base: Duration::from_millis(200),
            default_limits: KeyLimits::none(),
        }
    }
}

impl PoolConfig {
    /// Joins the base URL with the `chat/completions` endpoint,
    /// tolerating a missing trailing slash.
    pub(crate) fn chat_completions_url(&self) -> String {
        format!("{}/chat/completions", self.default_url.trim_end_matches('/'))
    }
}
