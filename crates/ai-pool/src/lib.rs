//! # ai-pool
//!
//! A fault-tolerant client for OpenAI-compatible APIs with key pooling,
//! automatic rotation, per-key rate quotas, and encrypted local storage.
//! Built for applications that manage a pool of user-supplied API keys and
//! need to survive rate limits, revoked keys, and flaky upstreams without
//! bubbling every failure to the caller.
//!
//! ## Architecture
//!
//! The crate is split into three independent parts:
//!
//! 1. **Storage** ([`storage`], [`crypto`]) — keys are sealed with
//!    AES-256-GCM envelope encryption behind a pluggable [`KeyStore`]; the
//!    master key comes from the OS keychain, a custom source, or RAM.
//! 2. **Selection** ([`engine`]) — decrypted keys live in RAM; per request
//!    the pool picks a key by [`RotationMode::Proactive`] round-robin or
//!    [`RotationMode::Reactive`] sticky failover, honoring per-key quotas.
//! 3. **Dispatch** ([`http`]) — wraps `reqwest` with a fixed retry policy:
//!    429 puts the key in cooldown and rotates, 401/403 bans it and rotates,
//!    400/404/422 fail fast, 5xx retries the same key with backoff, and
//!    timeouts never trigger rotation.
//!
//! ## Quick start
//!
//! ```no_run
//! use ai_pool::{AiClient, RotationMode};
//! use std::time::Duration;
//!
//! # async fn run() -> Result<(), ai_pool::AiError> {
//! let client = AiClient::builder()
//!     .default_url("https://generativelanguage.googleapis.com/v1beta/openai/")
//!     .rotation_mode(RotationMode::Proactive)
//!     .concurrency_limit(50)
//!     .request_timeout(Duration::from_secs(60))
//!     .api_keys(vec!["key-1", "key-2"])
//!     .build()
//!     .await?;
//!
//! let resp = client.chat(serde_json::json!({
//!     "model": "gemini-2.0-flash",
//!     "messages": [{"role": "user", "content": "hello"}]
//! })).await?;
//! println!("{}", resp.choices[0].message.content);
//! # Ok(())
//! # }
//! ```
//!
//! ## Cargo features
//!
//! | feature       | adds                                             |
//! |---------------|--------------------------------------------------|
//! | `sqlite`      | encrypted `SqliteStore` backend (`sqlx`)         |
//! | `os-keychain` | `MasterKeyProvider::OsKeychain` (`keyring`)      |
//! | `stream`      | `chat_stream` SSE support (`futures-util`)       |

pub mod client;
pub mod config;
pub mod crypto;
pub mod engine;
pub mod error;
pub mod http;
pub mod quota;
pub mod sdk;
pub mod storage;

// --- Public surface -------------------------------------------------------

pub use client::{AiClient, AiClientBuilder};
pub use sdk::{ChatBuilder, ReasoningEffort, ResponseFormat, ThinkingConfig, Unstructured};
pub use config::{PoolConfig, RotationMode};
pub use engine::KeySeed;
pub use quota::{KeyLimits, KeyQuotaInfo, WindowInfo, WindowState};
pub use crypto::MasterKeyProvider;
pub use error::{AiError, VaultError};
pub use http::models::{ChatResponse, Choice, Message, Usage};
pub use storage::{KeyHealth, KeyManager, KeyMetadata, KeyRecord, KeyStatus, KeyStore};

#[cfg(feature = "stream")]
pub use error::StreamInterruption;
#[cfg(feature = "stream")]
pub use http::models::{ChatChunk, ChunkChoice, Delta};
#[cfg(feature = "stream")]
pub use http::stream::ChatStream;

// Re-export secrecy so downstream code can call `.expose_secret()` without
// adding the dependency themselves.
pub use secrecy::{ExposeSecret, SecretBox, SecretString};

// Re-export schemars so downstream code can `#[derive(JsonSchema)]` for
// `ChatBuilder::response_format_schema` without adding the dependency.
pub use schemars::{self, JsonSchema};
