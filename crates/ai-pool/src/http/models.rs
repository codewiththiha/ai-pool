//! OpenAI-compatible JSON/SSE structs.

use serde::{Deserialize, Serialize};

/// Non-streaming `chat/completions` response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatResponse {
    /// Provider-assigned completion id.
    #[serde(default)]
    pub id: String,
    /// Model that produced the completion.
    #[serde(default)]
    pub model: String,
    /// Generated choices (usually one).
    pub choices: Vec<Choice>,
    /// Token accounting, if the provider reports it.
    #[serde(default)]
    pub usage: Option<Usage>,
}

/// One generated completion choice.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Choice {
    /// Position of this choice in the response.
    #[serde(default)]
    pub index: u32,
    /// The generated message.
    pub message: Message,
    /// Why generation stopped (`stop`, `length`, ...).
    #[serde(default)]
    pub finish_reason: Option<String>,
}

/// A chat message (request or response).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    /// `system`, `user`, `assistant`, ...
    pub role: String,
    /// Message text content.
    #[serde(default)]
    pub content: String,
}

/// Token usage accounting.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Usage {
    /// Tokens consumed by the prompt.
    #[serde(default)]
    pub prompt_tokens: u64,
    /// Tokens generated in the completion.
    #[serde(default)]
    pub completion_tokens: u64,
    /// Total tokens billed.
    #[serde(default)]
    pub total_tokens: u64,
}

/// One SSE chunk of a streaming completion.
#[cfg(feature = "stream")]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatChunk {
    /// Provider-assigned completion id.
    #[serde(default)]
    pub id: String,
    /// Model that produced the chunk.
    #[serde(default)]
    pub model: String,
    /// Incremental choice deltas.
    #[serde(default)]
    pub choices: Vec<ChunkChoice>,
}

/// One choice inside a streaming chunk.
#[cfg(feature = "stream")]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkChoice {
    /// Position of this choice in the response.
    #[serde(default)]
    pub index: u32,
    /// Incremental content update.
    #[serde(default)]
    pub delta: Delta,
    /// Why generation stopped, present only on the final chunk.
    #[serde(default)]
    pub finish_reason: Option<String>,
}

/// Incremental message fragment inside a streaming choice.
#[cfg(feature = "stream")]
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Delta {
    /// Role, present only on the first chunk.
    #[serde(default)]
    pub role: Option<String>,
    /// Text fragment to append.
    #[serde(default)]
    pub content: Option<String>,
}

/// Error object a provider may return in a body — or inject mid-stream.
#[derive(Debug, Clone, Deserialize)]
pub struct ApiErrorEnvelope {
    /// The wrapped error body.
    pub error: ApiErrorBody,
}

/// Provider error details.
#[derive(Debug, Clone, Deserialize)]
pub struct ApiErrorBody {
    /// Human-readable error message.
    #[serde(default)]
    pub message: String,
    /// Providers send this as a number (`"code": 429`) or string
    /// (`"code": "429"` / `"rate_limit_exceeded"`), so accept anything.
    #[serde(default)]
    pub code: Option<serde_json::Value>,
    /// Provider-specific error type tag.
    #[serde(default, rename = "type")]
    pub kind: Option<String>,
    /// Alternative status field some providers use.
    #[serde(default)]
    pub status: Option<serde_json::Value>,
}

impl ApiErrorBody {
    /// Best-effort numeric code extraction from `code`/`status` fields.
    pub fn numeric_code(&self) -> Option<u16> {
        for v in [self.code.as_ref(), self.status.as_ref()]
            .into_iter()
            .flatten()
        {
            match v {
                serde_json::Value::Number(n) => {
                    if let Some(u) = n.as_u64() {
                        return u16::try_from(u).ok();
                    }
                }
                serde_json::Value::String(s) => {
                    if let Ok(u) = s.parse::<u16>() {
                        return Some(u);
                    }
                }
                _ => {}
            }
        }
        // Fall back on well-known symbolic identifiers.
        let hay = format!(
            "{} {}",
            self.kind.as_deref().unwrap_or(""),
            self.code.as_ref().and_then(|c| c.as_str()).unwrap_or("")
        )
        .to_lowercase();
        if hay.contains("rate_limit") || hay.contains("resource_exhausted") {
            Some(429)
        } else if hay.contains("invalid_api_key") || hay.contains("unauthenticated") {
            Some(401)
        } else if hay.contains("permission") {
            Some(403)
        } else {
            None
        }
    }
}
