//! [`ChatBuilder`]: `send` returns a [`ChatResponse`] until a schema types it.

use std::marker::PhantomData;

use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};

use super::strict::{make_strict, schema_name};
use crate::client::AiClient;
use crate::error::AiError;
use crate::http::models::{ChatResponse, Choice, Message};

#[cfg(feature = "stream")]
use crate::http::stream::ChatStream;

/// Marker: `send` returns the raw [`ChatResponse`].
#[derive(Debug, Clone, Copy, Default)]
pub struct Unstructured;

/// How hard a reasoning model thinks; sent as `reasoning_effort`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum ReasoningEffort {
    /// No reasoning where the model allows it (Gemini 2.5 Flash, GPT-5.1).
    None,
    /// The least reasoning a model that always reasons supports.
    Minimal,
    /// Light reasoning.
    Low,
    /// Balanced reasoning.
    Medium,
    /// Thorough reasoning.
    High,
}

impl ReasoningEffort {
    const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }
}

/// Reasoning controls; providers differ, so pick the form yours reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ThinkingConfig {
    /// Sends `"reasoning_effort": null`.
    Disabled,
    /// Sends a `thinking` object with an optional `thinking_budget`.
    Enabled {
        /// Token budget for reasoning.
        budget_tokens: Option<u32>,
    },
    /// Sends `reasoning_effort`.
    Effort(ReasoningEffort),
}

/// The shape of the answer.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ResponseFormat {
    /// Free text.
    Text,
    /// Any JSON object; the prompt must describe the shape.
    Json,
    /// JSON matching `schema`.
    JsonSchema {
        /// Schema name; sanitised to what the API accepts.
        name: String,
        /// A JSON Schema; property descriptions act as per-field prompts.
        schema: Value,
        /// Close objects and require every field; providers then enforce it.
        strict: bool,
    },
}

impl ResponseFormat {
    /// A strict JSON Schema format.
    pub fn json_schema(name: impl Into<String>, schema: Value) -> Self {
        Self::JsonSchema {
            name: name.into(),
            schema,
            strict: true,
        }
    }
}

/// A chat request under construction; see the module docs.
#[must_use = "call one of the send methods to execute the request"]
#[derive(Clone)]
pub struct ChatBuilder<'a, T = Unstructured> {
    pub(super) client: &'a AiClient,
    pub(super) model: Option<String>,
    pub(super) messages: Vec<Message>,
    temperature: Option<f64>,
    top_p: Option<f64>,
    max_tokens: Option<u32>,
    max_completion_tokens: Option<u32>,
    thinking: Option<ThinkingConfig>,
    pub(super) response_format: Option<ResponseFormat>,
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

impl<'a, T> ChatBuilder<'a, T> {
    pub(crate) fn new(client: &'a AiClient) -> Self {
        Self {
            client,
            model: None,
            messages: Vec::new(),
            temperature: None,
            top_p: None,
            max_tokens: None,
            max_completion_tokens: None,
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
            refusal: None,
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

    /// Cap as `max_completion_tokens`, which `OpenAI` reasoning models need.
    pub const fn max_completion_tokens(mut self, max: u32) -> Self {
        self.max_completion_tokens = Some(max);
        self
    }

    /// Configures model reasoning. See [`ThinkingConfig`] for how each
    /// variant maps onto provider-specific JSON.
    pub const fn thinking(mut self, config: ThinkingConfig) -> Self {
        self.thinking = Some(config);
        self
    }

    /// Sets a [`ResponseFormat`] without typing the builder.
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

    /// Asks for JSON matching any JSON Schema; `send` parses it into `U`.
    pub fn response_json_schema<U: DeserializeOwned>(
        self,
        name: impl Into<String>,
        schema: Value,
    ) -> ChatBuilder<'a, U> {
        self.typed(ResponseFormat::json_schema(name, schema))
    }

    /// Asks for any JSON object, for servers without schema support.
    pub fn response_json<U: DeserializeOwned>(self) -> ChatBuilder<'a, U> {
        self.typed(ResponseFormat::Json)
    }

    /// Asks for JSON matching `U`'s derived schema; doc comments guide fields.
    #[cfg(feature = "derive")]
    pub fn response_format_schema<U: schemars::JsonSchema + DeserializeOwned>(
        self,
        name: impl Into<String>,
    ) -> ChatBuilder<'a, U> {
        let root = schemars::schema_for!(U);
        let schema = serde_json::to_value(root).unwrap_or_else(|_| json!({}));
        self.response_json_schema(name, schema)
    }

    fn typed<U>(self, format: ResponseFormat) -> ChatBuilder<'a, U> {
        ChatBuilder {
            client: self.client,
            model: self.model,
            messages: self.messages,
            temperature: self.temperature,
            top_p: self.top_p,
            max_tokens: self.max_tokens,
            max_completion_tokens: self.max_completion_tokens,
            thinking: self.thinking,
            response_format: Some(format),
            extra: self.extra,
            _output: PhantomData,
        }
    }

    /// The OpenAI-compatible JSON body this builder sends.
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
        if let Some(m) = self.max_completion_tokens {
            payload.insert("max_completion_tokens".into(), json!(m));
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
        if let Some(format) = &self.response_format {
            payload.insert("response_format".into(), format_json(format));
        }
        for (k, v) in &self.extra {
            payload.insert(k.clone(), v.clone());
        }
        Value::Object(payload)
    }
}

fn format_json(format: &ResponseFormat) -> Value {
    match format {
        ResponseFormat::Text => json!({"type": "text"}),
        ResponseFormat::Json => json!({"type": "json_object"}),
        ResponseFormat::JsonSchema {
            name,
            schema,
            strict,
        } => {
            let mut schema = schema.clone();
            if *strict {
                make_strict(&mut schema);
            }
            if let Some(obj) = schema.as_object_mut() {
                obj.remove("$schema");
            }
            json!({
                "type": "json_schema",
                "json_schema": { "name": schema_name(name), "strict": strict, "schema": schema }
            })
        }
    }
}

/// The first choice, or why there is no usable one.
pub(super) fn first_choice(resp: ChatResponse) -> Result<Choice, AiError> {
    let choice = resp
        .choices
        .into_iter()
        .next()
        .ok_or_else(|| AiError::InvalidResponse("response contained no choices".into()))?;
    if let Some(refusal) = &choice.message.refusal {
        return Err(AiError::Refused(refusal.clone()));
    }
    Ok(choice)
}

pub(super) fn cut_off(choice: &Choice) -> bool {
    choice.finish_reason.as_deref() == Some("length")
}

impl ChatBuilder<'_, Unstructured> {
    /// Sends the request and returns the whole [`ChatResponse`].
    pub async fn send(self) -> Result<ChatResponse, AiError> {
        let payload = self.into_json();
        self.client.chat(payload).await
    }

    /// Sends the request and returns the first choice's text.
    pub async fn send_text(self) -> Result<String, AiError> {
        let choice = first_choice(self.send().await?)?;
        if choice.message.content.is_empty() && cut_off(&choice) {
            return Err(AiError::Truncated);
        }
        Ok(choice.message.content)
    }
}

impl<T: DeserializeOwned> ChatBuilder<'_, T> {
    /// Sends and parses the answer into `T`, past fences and prose.
    pub async fn send(self) -> Result<T, AiError> {
        let payload = self.into_json();
        let choice = first_choice(self.client.chat(payload).await?)?;
        let text = choice.message.content.as_str();
        parse_json::<T>(text).map_err(|e| {
            if cut_off(&choice) {
                AiError::Truncated
            } else {
                AiError::InvalidResponse(format!(
                    "failed to parse structured output: {e}; content: {}",
                    crate::http::dispatcher::truncate(text, 300)
                ))
            }
        })
    }
}

#[cfg(feature = "stream")]
impl<T> ChatBuilder<'_, T> {
    /// Sends the request as a stream of [`crate::ChatChunk`]s.
    pub async fn send_stream(self) -> Result<ChatStream, AiError> {
        let payload = self.into_json();
        self.client.chat_stream(payload).await
    }
}

/// Parses `text` as `T`, else its fenced or outermost JSON value.
fn parse_json<T: DeserializeOwned>(text: &str) -> Result<T, serde_json::Error> {
    let fenced = strip_code_fences(text);
    serde_json::from_str::<T>(fenced).or_else(|first| {
        outermost_json(fenced)
            .and_then(|inner| serde_json::from_str::<T>(inner).ok())
            .ok_or(first)
    })
}

/// The span from the first `{` or `[` to the last matching closer.
fn outermost_json(text: &str) -> Option<&str> {
    let start = text.find(['{', '['])?;
    let closer = if text.get(start..)?.starts_with('{') {
        '}'
    } else {
        ']'
    };
    let end = text.rfind(closer)?;
    text.get(start..=end)
}

/// Strips the ```` ```json ```` fences some models wrap JSON in.
fn strip_code_fences(text: &str) -> &str {
    let trimmed = text.trim();
    let Some(rest) = trimmed.strip_prefix("```") else {
        return trimmed;
    };
    let rest = rest.split_once('\n').map_or(rest, |(_, body)| body);
    rest.strip_suffix("```").unwrap_or(rest).trim()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, serde::Deserialize, PartialEq)]
    struct Word {
        word: String,
    }

    #[test]
    fn json_is_found_inside_prose() {
        let parsed: Word = parse_json("Sure! {\"word\": \"ox\"} Hope that helps.").unwrap();
        assert_eq!(parsed.word, "ox");
        assert!(parse_json::<Word>("no json here").is_err());
        assert!(parse_json::<Word>("} backwards {").is_err());
    }

    #[test]
    fn strip_fences() {
        assert_eq!(strip_code_fences("{\"a\":1}"), "{\"a\":1}");
        assert_eq!(strip_code_fences("```json\n{\"a\":1}\n```"), "{\"a\":1}");
        assert_eq!(strip_code_fences("```\n{\"a\":1}\n```"), "{\"a\":1}");
    }
}
