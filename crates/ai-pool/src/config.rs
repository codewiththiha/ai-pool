//! Pool configuration and rotation strategies.

use std::time::Duration;

use crate::quota::KeyLimits;

/// How the pool picks the key for the next request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RotationMode {
    /// Round-robin over usable keys, spreading load evenly.
    #[default]
    Proactive,
    /// Stay on one key until it fails or saturates, then fail over.
    Reactive,
}

/// How a key travels with each request.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Auth {
    /// `Authorization: Bearer <key>`, as `OpenAI` does.
    #[default]
    Bearer,
    /// The raw key in a named header, e.g. `api-key` or `x-goog-api-key`.
    Header(String),
    /// No key at all: local servers such as Ollama or LM Studio.
    None,
}

/// Every tunable of a client. The builder fills it.
#[derive(Debug, Clone)]
pub struct PoolConfig {
    /// Base URL of the OpenAI-compatible API, up to and including `/v1`.
    pub default_url: String,
    /// Key selection strategy.
    pub rotation_mode: RotationMode,
    /// How keys are sent.
    pub auth: Auth,
    /// Headers added to every request, e.g. `OpenRouter`'s `X-Title`.
    pub headers: Vec<(String, String)>,
    /// Requests in flight at once, across all keys.
    pub concurrency_limit: usize,
    /// Whole-request deadline.
    pub request_timeout: Duration,
    /// Deadline for establishing a connection.
    pub connect_timeout: Duration,
    /// How often a 5xx or 408 is retried on the same key.
    pub max_server_error_retries: u32,
    /// Cooldown after a 429 without a usable `Retry-After`.
    pub default_cooldown: Duration,
    /// First backoff step for same-key retries; doubles each time.
    pub backoff_base: Duration,
    /// Limits for keys without their own.
    pub default_limits: KeyLimits,
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            default_url: "https://api.openai.com/v1/".to_string(),
            rotation_mode: RotationMode::Proactive,
            auth: Auth::Bearer,
            headers: Vec::new(),
            concurrency_limit: 32,
            request_timeout: Duration::from_secs(60),
            connect_timeout: Duration::from_secs(10),
            max_server_error_retries: 3,
            default_cooldown: Duration::from_secs(30),
            backoff_base: Duration::from_millis(200),
            default_limits: KeyLimits::none(),
        }
    }
}

impl PoolConfig {
    fn endpoint(&self, path: &str) -> String {
        format!("{}/{path}", self.default_url.trim().trim_end_matches('/'))
    }

    pub(crate) fn chat_completions_url(&self) -> String {
        self.endpoint("chat/completions")
    }

    pub(crate) fn models_url(&self) -> String {
        self.endpoint("models")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoints_tolerate_trailing_slashes_and_spaces() {
        for url in ["http://h/v1", "http://h/v1/", " http://h/v1// "] {
            let config = PoolConfig {
                default_url: url.into(),
                ..PoolConfig::default()
            };
            assert_eq!(
                config.chat_completions_url(),
                "http://h/v1/chat/completions"
            );
            assert_eq!(config.models_url(), "http://h/v1/models");
        }
    }
}
