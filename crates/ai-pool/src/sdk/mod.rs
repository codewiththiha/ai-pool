//! A fluent, type-safe request builder, so callers never have to hand-write
//! raw `serde_json::Value` payloads.
//!
//! The builder is a pure convenience layer: it assembles an OpenAI-compatible
//! payload and hands it to [`crate::AiClient::chat`] /
//! [`crate::AiClient::chat_stream`]. Retry logic, SSE parsing, and key
//! storage are untouched.
//!
//! # Field-level prompting
//!
//! [`schemars`] turns Rust doc comments into `"description"` fields inside
//! the generated JSON schema, which the model reads as per-field
//! instructions:
//!
//! ```no_run
//! use ai_pool::{AiClient, JsonSchema, ThinkingConfig, ReasoningEffort};
//! use serde::Deserialize;
//!
//! #[derive(Debug, Deserialize, JsonSchema)]
//! struct SentimentAnalysis {
//!     /// MUST be exactly "positive", "negative", or "neutral".
//!     sentiment: String,
//!     /// A confidence score between 0.0 and 1.0.
//!     confidence: f64,
//!     /// One-sentence summary, written STRICTLY in formal French.
//!     summary: String,
//! }
//!
//! # async fn run(client: &AiClient) -> Result<(), ai_pool::AiError> {
//! let result: SentimentAnalysis = client.chat_builder()
//!     .model("gemini-2.0-flash")
//!     .user("The product arrived two days early and works great.")
//!     .thinking(ThinkingConfig::Effort(ReasoningEffort::High))
//!     .response_format_schema::<SentimentAnalysis>("sentiment_schema")
//!     .send() // no turbofish needed: the builder is now typed to T
//!     .await?;
//! # Ok(())
//! # }
//! ```

mod builder;

pub use builder::{ChatBuilder, ReasoningEffort, ResponseFormat, ThinkingConfig, Unstructured};
