//! The [`ChatBuilder`] type-state builder.
//!
//! The second type parameter `T` tracks what [`ChatBuilder::send`] returns:
//!
//! - `ChatBuilder<'_, Unstructured>` (the default from
//!   [`crate::AiClient::chat_builder`]) — `send()` returns the raw
//!   [`ChatResponse`]; `send_text()` returns the first choice's content.
//! - `ChatBuilder<'_, U>` after calling
//!   [`ChatBuilder::response_format_schema::<U>`] — `send()` deserializes the
//!   model's JSON reply straight into `U`.

use std::marker::PhantomData;

use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};

use crate::client::AiClient;
use crate::error::AiError;
use crate::http::models::{ChatResponse, Message};

#[cfg(feature = "stream")]
use crate::http::stream::ChatStream;

/// Type-state marker for a builder without a structured-output schema.
///
/// Deliberately **not** `Deserialize`: this is what lets the compiler give
/// `send()` different return types for structured vs. unstructured builders
/// without overlapping impls.
#[derive(Debug, Clone, Copy, Default)]
pub struct Unstructured;

/// Reasoning effort level for OpenAI-style reasoning models (o1/o3/gpt-5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningEffort {
    /// Low reasoning effort (fastest, cheapest).
    Low,
    /// Medium reasoning effort.
    Medium,
    /// High reasoning effort (deepest thinking).
    High,
}

impl ReasoningEffort {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }
}

/// How model reasoning ("thinking") should be requested from the provider.
///
/// The builder translates each variant into the exact JSON the major
/// OpenAI-compatible providers expect:
///
/// | Variant | Injected JSON |
/// |---|---|
/// | `Effort(High)` | `"reasoning_effort": "high"` (OpenAI o1/o3) |
/// | `Enabled { budget_tokens: Some(1024) }` | `"thinking": {"thinking_budget": 1024}` (Gemini / DeepSeek) |
/// | `Enabled { budget_tokens: None }` | `"thinking": {}` |
/// | `Disabled` | `"reasoning_effort": null` (explicitly off) |
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ThinkingConfig {
    /// Explicitly disable reasoning on models that default to it.
    Disabled,
    /// Gemini/DeepSeek-style thinking block with an optional token budget.
    Enabled {
        /// Maximum tokens the model may spend thinking, if supported.
        budget_tokens: Option<u32>,
    },
    /// OpenAI-style `reasoning_effort` knob.
    Effort(ReasoningEffort),
}

/// Desired response format.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ResponseFormat {
    /// Plain text (`{"type": "text"}`).
    Text,
    /// Any valid JSON object (`{"type": "json_object"}`).
    Json,
    /// Strict JSON conforming to a schema (`{"type": "json_schema", ...}`).
    JsonSchema {
        /// Schema name reported to the provider.
        name: String,
        /// The JSON schema document.
        schema: Value,
    },
}

/// Fluent, type-safe request builder returned by [`AiClient::chat_builder`].
#[must_use = "call one of the send methods to execute the request"]
#[derive(Clone)]
pub struct ChatBuilder<'a, T = Unstructured> {
    client: &'a AiClient,
    model: Option<String>,
    messages: Vec<Message>,
    temperature: Option<f64>,
    top_p: Option<f64>,
    max_tokens: Option<u32>,
    thinking: Option<ThinkingConfig>,
    response_format: Option<ResponseFormat>,
    extra: Map<String, Value>,
    _output: PhantomData<fn() -> T>,
}

impl<T> std::fmt::Debug for ChatBuilder<'_, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChatBuilder")
            .field("model", &self.model)
            .field("messages", &self.messages.len())
            .field("temperature", &self.temperature)
            .field("top_p", &self.top_p)
            .field("max_tokens", &self.max_tokens)
            .field("thinking", &self.thinking)
            .field("response_format", &self.response_format)
            .finish_non_exhaustive()
    }
}

// ---------------------------------------------------------------------------
// Fluent configuration (available in every type-state)
// ---------------------------------------------------------------------------

impl<'a, T> ChatBuilder<'a, T> {
    pub(crate) fn new(client: &'a AiClient) -> Self {
        Self {
            client,
            model: None,
            messages: Vec::new(),
            temperature: None,
            top_p: None,
            max_tokens: None,
            thinking: None,
            response_format: None,
            extra: Map::new(),
            _output: PhantomData,
        }
    }

    /// Sets the model id, e.g. `"gemini-2.0-flash"` or `"gpt-4o-mini"`.
    pub fn model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    /// Appends a `system` message.
    pub fn system(self, content: impl Into<String>) -> Self {
        self.message("system", content)
    }

    /// Appends a `user` message.
    pub fn user(self, content: impl Into<String>) -> Self {
        self.message("user", content)
    }

    /// Appends an `assistant` message (for few-shot / history replay).
    pub fn assistant(self, content: impl Into<String>) -> Self {
        self.message("assistant", content)
    }

    /// Appends a message with an arbitrary role.
    pub fn message(mut self, role: impl Into<String>, content: impl Into<String>) -> Self {
        self.messages.push(Message {
            role: role.into(),
            content: content.into(),
        });
        self
    }

    /// Sampling temperature (0.0 = deterministic, ~2.0 = wild).
    pub const fn temperature(mut self, temperature: f64) -> Self {
        self.temperature = Some(temperature);
        self
    }

    /// Nucleus sampling cutoff.
    pub const fn top_p(mut self, top_p: f64) -> Self {
        self.top_p = Some(top_p);
        self
    }

    /// Hard cap on generated tokens.
    pub const fn max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = Some(max_tokens);
        self
    }

    /// Configures model reasoning. See [`ThinkingConfig`] for how each
    /// variant maps onto provider-specific JSON.
    pub const fn thinking(mut self, config: ThinkingConfig) -> Self {
        self.thinking = Some(config);
        self
    }

    /// Sets an explicit [`ResponseFormat`] without changing the output type.
    ///
    /// Prefer [`Self::response_format_schema`] for schema-driven typed
    /// output; use this for `Text` / `Json` or a hand-built schema.
    pub fn response_format(mut self, format: ResponseFormat) -> Self {
        self.response_format = Some(format);
        self
    }

    /// Escape hatch: injects an arbitrary top-level field into the payload
    /// (e.g. provider-specific extensions like `"safety_settings"`).
    pub fn extra(mut self, key: impl Into<String>, value: Value) -> Self {
        self.extra.insert(key.into(), value);
        self
    }

    /// Requests **structured output** conforming to the JSON schema derived
    /// from `U` (via `#[derive(JsonSchema)]`, re-exported from [`schemars`]).
    ///
    /// Doc comments on `U`'s fields become `"description"` entries in the
    /// schema — per-field prompts the model follows when filling them in.
    ///
    /// Returns a builder **typed to `U`**, so a later
    /// [`ChatBuilder::send`] deserializes the reply into `U` with no
    /// turbofish at the call site.
    pub fn response_format_schema<U: schemars::JsonSchema>(
        self,
        name: impl Into<String>,
    ) -> ChatBuilder<'a, U> {
        let root = schemars::schema_for!(U);
        let mut schema = serde_json::to_value(root.schema).unwrap_or_else(|_| json!({}));
        // Strict mode requires additionalProperties: false on objects.
        if let Some(obj) = schema.as_object_mut() {
            obj.entry("additionalProperties").or_insert(json!(false));
        }
        ChatBuilder {
            client: self.client,
            model: self.model,
            messages: self.messages,
            temperature: self.temperature,
            top_p: self.top_p,
            max_tokens: self.max_tokens,
            thinking: self.thinking,
            response_format: Some(ResponseFormat::JsonSchema {
                name: name.into(),
                schema,
            }),
            extra: self.extra,
            _output: PhantomData,
        }
    }

    /// Assembles the final OpenAI-compatible JSON payload.
    pub fn into_json(&self) -> Value {
        let mut payload = Map::new();
        if let Some(model) = &self.model {
            payload.insert("model".into(), json!(model));
        }
        payload.insert(
            "messages".into(),
            serde_json::to_value(&self.messages).unwrap_or_else(|_| json!([])),
        );
        if let Some(t) = self.temperature {
            payload.insert("temperature".into(), json!(t));
        }
        if let Some(p) = self.top_p {
            payload.insert("top_p".into(), json!(p));
        }
        if let Some(m) = self.max_tokens {
            payload.insert("max_tokens".into(), json!(m));
        }
        match self.thinking {
            Some(ThinkingConfig::Effort(effort)) => {
                payload.insert("reasoning_effort".into(), json!(effort.as_str()));
            }
            Some(ThinkingConfig::Enabled { budget_tokens }) => {
                let mut thinking = Map::new();
                if let Some(budget) = budget_tokens {
                    thinking.insert("thinking_budget".into(), json!(budget));
                }
                payload.insert("thinking".into(), Value::Object(thinking));
            }
            Some(ThinkingConfig::Disabled) => {
                payload.insert("reasoning_effort".into(), Value::Null);
            }
            None => {}
        }
        match &self.response_format {
            Some(ResponseFormat::Text) => {
                payload.insert("response_format".into(), json!({"type": "text"}));
            }
            Some(ResponseFormat::Json) => {
                payload.insert("response_format".into(), json!({"type": "json_object"}));
            }
            Some(ResponseFormat::JsonSchema { name, schema }) => {
                payload.insert(
                    "response_format".into(),
                    json!({
                        "type": "json_schema",
                        "json_schema": {
                            "name": name,
                            "strict": true,
                            "schema": schema,
                        }
                    }),
                );
            }
            None => {}
        }
        for (k, v) in &self.extra {
            payload.insert(k.clone(), v.clone());
        }
        Value::Object(payload)
    }
}

// ---------------------------------------------------------------------------
// Execution: unstructured state
// ---------------------------------------------------------------------------

impl ChatBuilder<'_, Unstructured> {
    /// Executes the request and returns the full [`ChatResponse`].
    pub async fn send(self) -> Result<ChatResponse, AiError> {
        let payload = self.into_json();
        self.client.chat(payload).await
    }

    /// Executes the request and returns the first choice's text content.
    pub async fn send_text(self) -> Result<String, AiError> {
        let resp = self.send().await?;
        resp.choices
            .into_iter()
            .next()
            .map(|c| c.message.content)
            .ok_or_else(|| AiError::InvalidResponse("response contained no choices".into()))
    }
}

// ---------------------------------------------------------------------------
// Execution: schema-typed state
// ---------------------------------------------------------------------------

impl<T: DeserializeOwned> ChatBuilder<'_, T> {
    /// Executes the request and deserializes the model's JSON reply into `T`.
    ///
    /// Tolerates replies wrapped in markdown code fences. Parsing failures
    /// map to [`AiError::InvalidResponse`].
    pub async fn send(self) -> Result<T, AiError> {
        let payload = self.into_json();
        let resp = self.client.chat(payload).await?;
        let text = resp
            .choices
            .into_iter()
            .next()
            .map(|c| c.message.content)
            .ok_or_else(|| AiError::InvalidResponse("response contained no choices".into()))?;
        let cleaned = strip_code_fences(&text);
        serde_json::from_str::<T>(cleaned).map_err(|e| {
            AiError::InvalidResponse(format!(
                "failed to parse structured output: {e}; content: {}",
                crate::http::dispatcher::truncate(cleaned, 300)
            ))
        })
    }
}

// ---------------------------------------------------------------------------
// Execution: streaming (any state — chunks are always incremental text)
// ---------------------------------------------------------------------------

#[cfg(feature = "stream")]
impl<T> ChatBuilder<'_, T> {
    /// Executes the request as an SSE stream of [`crate::ChatChunk`]s.
    pub async fn send_stream(self) -> Result<ChatStream, AiError> {
        let payload = self.into_json();
        self.client.chat_stream(payload).await
    }
}

/// Strips ```` ```json ... ``` ```` fences some models wrap JSON in.
fn strip_code_fences(text: &str) -> &str {
    let trimmed = text.trim();
    let Some(rest) = trimmed.strip_prefix("```") else {
        return trimmed;
    };
    // Drop the language tag line (e.g. "json").
    let rest = rest.split_once('\n').map_or(rest, |(_, body)| body);
    rest.strip_suffix("```").unwrap_or(rest).trim()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_fences() {
        assert_eq!(strip_code_fences("{\"a\":1}"), "{\"a\":1}");
        assert_eq!(strip_code_fences("```json\n{\"a\":1}\n```"), "{\"a\":1}");
        assert_eq!(strip_code_fences("```\n{\"a\":1}\n```"), "{\"a\":1}");
    }
}
