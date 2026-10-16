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
#[derive(Debug, Clone, Serialize)]
pub struct Message {
    /// `system`, `user`, `assistant`, ...
    pub role: String,
    /// Text content; arrays of content parts are joined, non-text skipped.
    pub content: String,
    /// The model's explanation when it declined a structured request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refusal: Option<String>,
}

/// One `{type, text}` part of an array-shaped `content`.
#[derive(Debug, Clone, Deserialize)]
struct ContentPart {
    #[serde(default, rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    text: Option<String>,
}

impl<'de> serde::Deserialize<'de> for Message {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        // Content is a string or an array of parts.
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Content {
            Text(String),
            Parts(Vec<ContentPart>),
        }

        #[derive(Deserialize)]
        struct Raw {
            role: String,
            #[serde(default)]
            content: Option<Content>,
            #[serde(default)]
            refusal: Option<String>,
        }

        let raw = Raw::deserialize(deserializer)?;
        let content = match raw.content {
            None => String::new(),
            Some(Content::Text(text)) if text.is_empty() => String::new(),
            Some(Content::Text(text)) => text,
            Some(Content::Parts(parts)) => parts
                .into_iter()
                .filter(|p| p.kind.as_deref().is_none_or(|k| k == "text"))
                .filter_map(|p| p.text)
                .collect::<String>(),
        };
        Ok(Self {
            role: raw.role,
            content,
            refusal: raw.refusal.filter(|r| !r.trim().is_empty()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserializes_plain_string_content() {
        let m: Message = serde_json::from_str(r#"{"role":"assistant","content":"hello"}"#).unwrap();
        assert_eq!(m.role, "assistant");
        assert_eq!(m.content, "hello");
    }

    #[test]
    fn deserializes_parts_array_content() {
        let m: Message = serde_json::from_str(
            r#"{"role":"assistant","content":[{"type":"text","text":"Hel"},{"type":"text","text":"lo"}]}"#,
        )
        .unwrap();
        assert_eq!(m.content, "Hello");
    }

    #[test]
    fn skips_non_text_parts() {
        let m: Message = serde_json::from_str(
            r#"{"role":"assistant","content":[{"type":"image_url","image_url":{"url":"x"}},{"type":"text","text":"ok"}]}"#,
        )
        .unwrap();
        assert_eq!(m.content, "ok");
    }

    #[test]
    fn missing_or_null_content_is_empty() {
        let m: Message = serde_json::from_str(r#"{"role":"assistant"}"#).unwrap();
        assert_eq!(m.content, "");
        let m: Message = serde_json::from_str(r#"{"role":"assistant","content":null}"#).unwrap();
        assert_eq!(m.content, "");
    }

    #[test]
    fn serializes_back_to_plain_string() {
        let m = Message {
            role: "user".into(),
            content: "hi".into(),
            refusal: None,
        };
        assert_eq!(
            serde_json::to_string(&m).unwrap(),
            r#"{"role":"user","content":"hi"}"#
        );
    }
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
    /// The code as a number, from numeric, string or named forms.
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

/// One entry of `GET /models`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelInfo {
    /// The id to pass as `model`.
    pub id: String,
    /// The organisation the provider names, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owned_by: Option<String>,
}

/// `GET /models`, as OpenAI-compatible servers answer it.
#[derive(Debug, Deserialize)]
pub(crate) struct ModelList {
    #[serde(default)]
    data: Vec<ModelInfo>,
    /// Some servers answer with `models` instead of `data`.
    #[serde(default)]
    models: Vec<ModelInfo>,
}

impl ModelList {
    /// Every entry, ids without Gemini's `models/` prefix.
    pub(crate) fn into_models(self) -> Vec<ModelInfo> {
        self.data
            .into_iter()
            .chain(self.models)
            .map(|mut m| {
                if let Some(id) = m.id.strip_prefix("models/") {
                    m.id = id.to_string();
                }
                m
            })
            .filter(|m| !m.id.trim().is_empty())
            .collect()
    }
}

#[cfg(test)]
mod model_list_tests {
    use super::*;

    #[test]
    fn reads_openai_and_gemini_shapes() {
        let list: ModelList = serde_json::from_str(
            r#"{"object":"list","data":[{"id":"models/gemini-2.5-flash","owned_by":"google"},{"id":""}]}"#,
        )
        .unwrap();
        let models = list.into_models();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "gemini-2.5-flash");
    }

    #[test]
    fn refusals_are_kept_and_blank_ones_dropped() {
        let m: Message =
            serde_json::from_str(r#"{"role":"assistant","content":null,"refusal":"no"}"#).unwrap();
        assert_eq!(m.refusal.as_deref(), Some("no"));
        let m: Message =
            serde_json::from_str(r#"{"role":"assistant","content":"x","refusal":" "}"#).unwrap();
        assert_eq!(m.refusal, None);
    }
}
