//! Structured answers from any model, stepping down to what it honours.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::builder::{ChatBuilder, ResponseFormat, Unstructured, cut_off, first_choice};
use super::json;
use crate::client::AiClient;
use crate::error::AiError;
use crate::http::dispatcher::truncate;
use crate::http::models::Message;

/// More remembered models than this and the file stops growing.
const MAX_REMEMBERED: usize = 512;

/// How a model is asked for JSON, strictest first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum JsonMode {
    /// `json_schema` format; the schema is in the prompt too.
    Schema,
    /// `json_object` format; the schema is in the prompt.
    Object,
    /// No format: one user message asking for nothing but the JSON.
    Prompted,
}

impl JsonMode {
    /// The next, more forgiving mode.
    pub const fn weaker(self) -> Option<Self> {
        match self {
            Self::Schema => Some(Self::Object),
            Self::Object => Some(Self::Prompted),
            Self::Prompted => None,
        }
    }
}

/// Which [`JsonMode`] works for each server and model.
///
/// Shared by clients through `AiClientBuilder::json_modes`.
#[derive(Debug, Default)]
pub struct ModeMemory {
    modes: Mutex<HashMap<(String, String), JsonMode>>,
    file: Option<PathBuf>,
}

#[derive(Serialize, Deserialize)]
struct Entry {
    url: String,
    model: String,
    mode: JsonMode,
}

impl ModeMemory {
    /// Remembers for the life of the process.
    pub fn new() -> Self {
        Self::default()
    }

    /// Loads `path` now, blocking, and rewrites it on each change.
    pub fn open(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let entries: Vec<Entry> = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        let modes = entries
            .into_iter()
            .take(MAX_REMEMBERED)
            .map(|e| ((e.url, e.model), e.mode))
            .collect();
        Self {
            modes: Mutex::new(modes),
            file: Some(path),
        }
    }

    /// The mode that last worked for `model` at `url`.
    pub fn get(&self, url: &str, model: &str) -> Option<JsonMode> {
        let map = self.modes.lock().unwrap_or_else(PoisonError::into_inner);
        map.get(&(url.to_owned(), model.to_owned())).copied()
    }

    /// Records that `mode` works for `model` at `url`.
    pub fn remember(&self, url: &str, model: &str, mode: JsonMode) {
        let key = (url.to_owned(), model.to_owned());
        let mut map = self.modes.lock().unwrap_or_else(PoisonError::into_inner);
        let full = map.len() >= MAX_REMEMBERED && !map.contains_key(&key);
        if full || map.get(&key) == Some(&mode) {
            return;
        }
        map.insert(key, mode);
        let text = snapshot(&map);
        drop(map);
        self.save(text);
    }

    /// Starts `model` at `url` from the strictest mode again.
    pub fn forget(&self, url: &str, model: &str) {
        let mut map = self.modes.lock().unwrap_or_else(PoisonError::into_inner);
        if map.remove(&(url.to_owned(), model.to_owned())).is_none() {
            return;
        }
        let text = snapshot(&map);
        drop(map);
        self.save(text);
    }

    fn save(&self, text: String) {
        let Some(path) = self.file.clone() else {
            return;
        };
        match tokio::runtime::Handle::try_current() {
            Ok(rt) => {
                rt.spawn_blocking(move || write_atomic(&path, &text));
            }
            Err(_) => write_atomic(&path, &text),
        }
    }
}

fn snapshot(modes: &HashMap<(String, String), JsonMode>) -> String {
    let mut entries: Vec<Entry> = modes
        .iter()
        .map(|((url, model), mode)| Entry {
            url: url.clone(),
            model: model.clone(),
            mode: *mode,
        })
        .collect();
    entries.sort_by(|a, b| (&a.url, &a.model).cmp(&(&b.url, &b.model)));
    serde_json::to_string_pretty(&entries).unwrap_or_default()
}

/// Best effort: a lost write only costs one fallback later.
fn write_atomic(path: &Path, text: &str) {
    let tmp = path.with_extension("tmp");
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if std::fs::write(&tmp, text).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

/// A structured answer and how it was asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StructuredAnswer {
    /// The parsed JSON.
    pub value: Value,
    /// The mode that produced it.
    pub mode: JsonMode,
    /// Why `value` misses the schema, when even the last mode missed.
    pub mismatch: Option<String>,
}

/// What a structured stream reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StructuredEvent {
    /// The answer so far; later partials replace earlier ones.
    Partial(Value),
    /// The mode was refused or ignored; partials start over in this one.
    Retry(JsonMode),
    /// The finished answer.
    Done(StructuredAnswer),
}

/// Events of a structured stream; ends after `Done` or an error.
#[cfg(feature = "stream")]
pub type StructuredStream =
    std::pin::Pin<Box<dyn futures_util::Stream<Item = Result<StructuredEvent, AiError>> + Send>>;

/// A request for JSON matching a schema, from whatever the model honours.
#[must_use = "call `send` or `send_stream` to execute the request"]
#[derive(Debug, Clone)]
pub struct Structured<'a> {
    base: ChatBuilder<'a, Unstructured>,
    name: String,
    schema: Value,
    start: Option<JsonMode>,
}

impl<'a> ChatBuilder<'a, Unstructured> {
    /// Asks for JSON matching `schema`, falling back as the model needs.
    pub fn structured(self, name: impl Into<String>, schema: Value) -> Structured<'a> {
        Structured {
            base: self,
            name: name.into(),
            schema,
            start: None,
        }
    }
}

impl Structured<'_> {
    /// Starts at `mode` rather than the remembered or strictest one.
    pub const fn mode(mut self, mode: JsonMode) -> Self {
        self.start = Some(mode);
        self
    }

    /// The body sent in `mode`.
    pub fn payload(&self, mode: JsonMode) -> Value {
        let mut b = self.base.clone();
        match mode {
            JsonMode::Schema => {
                ground(&mut b.messages, &self.schema);
                let format = ResponseFormat::json_schema(self.name.as_str(), self.schema.clone());
                b.response_format = Some(format);
            }
            JsonMode::Object => {
                ground(&mut b.messages, &self.schema);
                b.response_format = Some(ResponseFormat::Json);
            }
            JsonMode::Prompted => {
                b.messages = prompted(&b.messages, &self.schema);
                b.response_format = None;
            }
        }
        b.into_json()
    }

    fn plan(&self) -> Plan {
        let client = self.base.client;
        let model = self.base.model.clone().unwrap_or_default();
        let url = client.base_url().to_owned();
        let first = self
            .start
            .or_else(|| client.json_modes().get(&url, &model))
            .unwrap_or(JsonMode::Schema);
        let mut tries = Vec::new();
        let mut mode = Some(first);
        while let Some(m) = mode {
            tries.push((m, self.payload(m)));
            mode = m.weaker();
        }
        Plan {
            client: client.clone(),
            url,
            model,
            schema: self.schema.clone(),
            tries,
        }
    }

    /// Sends, stepping down a mode each time one is refused or ignored.
    pub async fn send(self) -> Result<StructuredAnswer, AiError> {
        let plan = self.plan();
        for (mode, payload) in &plan.tries {
            let text = match plan.client.chat(payload.clone()).await {
                Ok(resp) => first_choice(resp).and_then(|c| {
                    if c.message.content.is_empty() && cut_off(&c) {
                        Err(AiError::Truncated)
                    } else {
                        Ok(c.message.content)
                    }
                }),
                Err(e) => Err(e),
            };
            match plan.judge(*mode, text) {
                Step::Next => {}
                Step::Done(answer) => return Ok(answer),
                Step::Fail(e) => return Err(e),
            }
        }
        Err(AiError::InvalidResponse("no mode left to try".into()))
    }

    /// Streams partial answers; a fallback restarts them.
    #[cfg(feature = "stream")]
    pub fn send_stream(self) -> StructuredStream {
        use futures_util::stream;

        let plan = self.plan();
        let (tx, rx) = tokio::sync::mpsc::channel(32);
        tokio::spawn(async move {
            let mut tries = plan.tries.iter().peekable();
            while let Some((mode, payload)) = tries.next() {
                let text = stream_text(&plan.client, payload.clone(), &tx).await;
                let event = match plan.judge(*mode, text) {
                    Step::Next => match tries.peek() {
                        Some((next, _)) => Ok(StructuredEvent::Retry(*next)),
                        None => Err(AiError::InvalidResponse("no mode left to try".into())),
                    },
                    Step::Done(answer) => Ok(StructuredEvent::Done(answer)),
                    Step::Fail(e) => Err(e),
                };
                let last = !matches!(event, Ok(StructuredEvent::Retry(_)));
                if tx.send(event).await.is_err() || last {
                    return;
                }
            }
        });
        Box::pin(stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|item| (item, rx))
        }))
    }
}

/// Every mode's request, worked out before any is sent.
struct Plan {
    client: AiClient,
    url: String,
    model: String,
    schema: Value,
    tries: Vec<(JsonMode, Value)>,
}

enum Step {
    Next,
    Done(StructuredAnswer),
    Fail(AiError),
}

impl Plan {
    fn judge(&self, mode: JsonMode, text: Result<String, AiError>) -> Step {
        let weaker = mode.weaker().is_some();
        let text = match text {
            Ok(text) => text,
            Err(e) if weaker && format_refused(&e) => return Step::Next,
            Err(e) => return Step::Fail(e),
        };
        let Some(value) = json::extract(&text) else {
            if weaker {
                return Step::Next;
            }
            let shown = truncate(&text, 300);
            return Step::Fail(AiError::InvalidResponse(format!("no JSON in: {shown}")));
        };
        let mismatch = match json::check(&value, &self.schema) {
            Ok(()) => None,
            Err(_) if weaker => return Step::Next,
            Err(why) => Some(why),
        };
        let memory = self.client.json_modes();
        memory.remember(&self.url, &self.model, mode);
        Step::Done(StructuredAnswer {
            value,
            mode,
            mismatch,
        })
    }
}

/// One streamed attempt's text; readable prefixes go out as partials.
#[cfg(feature = "stream")]
async fn stream_text(
    client: &AiClient,
    payload: Value,
    tx: &tokio::sync::mpsc::Sender<Result<StructuredEvent, AiError>>,
) -> Result<String, AiError> {
    use futures_util::StreamExt;

    let mut chunks = client.chat_stream(payload).await?;
    let mut text = String::new();
    let mut shown: Option<Value> = None;
    while let Some(chunk) = chunks.next().await {
        for choice in chunk?.choices {
            text.push_str(choice.delta.content.as_deref().unwrap_or_default());
            match choice.finish_reason.as_deref() {
                Some("length") => return Err(AiError::Truncated),
                Some("content_filter") => return Err(AiError::Refused(truncate(&text, 300))),
                _ => {}
            }
        }
        let now = json::partial(&text);
        if now.is_some() && now != shown {
            if let Some(v) = &now
                && tx
                    .send(Ok(StructuredEvent::Partial(v.clone())))
                    .await
                    .is_err()
            {
                return Err(AiError::InvalidResponse("the stream was dropped".into()));
            }
            shown = now;
        }
    }
    Ok(text)
}

/// Whether the server turned down the answer format, not the request.
pub fn format_refused(e: &AiError) -> bool {
    let AiError::BadRequest { status, body } = e else {
        return false;
    };
    let body = body.to_ascii_lowercase();
    matches!(status, 400 | 422)
        && ["response_format", "json_schema", "json_object", "schema"]
            .iter()
            .any(|w| body.contains(w))
}

/// Puts the schema in the system text, as servers that ignore it need.
fn ground(messages: &mut Vec<Message>, schema: &Value) {
    let rule = format!("Reply with one JSON object that matches this JSON Schema:\n{schema}");
    match messages.iter_mut().rev().find(|m| m.role == "system") {
        Some(system) => {
            system.content.push_str("\n\n");
            system.content.push_str(&rule);
        }
        None => messages.insert(0, message("system", rule)),
    }
}

/// The request as chat: system text first, the format rules last.
fn prompted(messages: &[Message], schema: &Value) -> Vec<Message> {
    let system: Vec<&str> = messages
        .iter()
        .filter(|m| m.role == "system")
        .map(|m| m.content.as_str())
        .collect();
    let mut out: Vec<Message> = messages
        .iter()
        .filter(|m| m.role != "system")
        .cloned()
        .collect();
    let rules = rules(schema);
    if !out.iter().any(|m| m.role == "user") {
        out.push(message("user", String::new()));
    }
    if let Some(first) = out.iter_mut().find(|m| m.role == "user")
        && !system.is_empty()
    {
        first.content = format!("{}\n\n{}", system.join("\n\n"), first.content);
    }
    if let Some(last) = out.iter_mut().rev().find(|m| m.role == "user") {
        last.content = format!("{}\n\n{rules}", last.content.trim_end());
    }
    out
}

fn rules(schema: &Value) -> String {
    let mut out = String::from(
        "Answer format:\n\
         - Reply with exactly one JSON object and nothing else.\n\
         - No markdown, no code fences, no comments, no text before or after it.\n",
    );
    if let Some(Value::Object(props)) = schema.get("properties") {
        let keys: Vec<String> = props.keys().map(|k| format!("\"{k}\"")).collect();
        let _ = writeln!(
            out,
            "- Use exactly these keys and fill in every one: {}.",
            keys.join(", ")
        );
    }
    let _ = write!(
        out,
        "- The object must match this JSON Schema:\n{schema}\n\
         - Its shape, with placeholder values:\n{}",
        json::skeleton(schema)
    );
    out
}

fn message(role: &str, content: String) -> Message {
    Message {
        role: role.to_owned(),
        content,
        refusal: None,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn schema() -> Value {
        json!({
            "type": "object",
            "properties": { "word": { "type": "string" } },
            "required": ["word"]
        })
    }

    #[test]
    fn only_format_complaints_step_down() {
        let bad = |status, body: &str| AiError::BadRequest {
            status,
            body: body.into(),
        };
        assert!(format_refused(&bad(
            400,
            "response_format json_schema unsupported"
        )));
        assert!(format_refused(&bad(
            400,
            "'response_format.type' must be 'json_schema' or 'text'"
        )));
        assert!(!format_refused(&bad(400, "model not found")));
        assert!(!format_refused(&AiError::Truncated));
    }

    #[test]
    fn grounding_joins_the_last_system_message() {
        let mut m = vec![
            message("system", "Be brief.".into()),
            message("user", "ox".into()),
        ];
        ground(&mut m, &schema());
        assert_eq!(m.len(), 2);
        assert!(
            m[0].content
                .starts_with("Be brief.\n\nReply with one JSON object")
        );
        let mut bare = vec![message("user", "ox".into())];
        ground(&mut bare, &schema());
        assert_eq!(bare[0].role, "system");
    }

    #[test]
    fn a_prompted_request_is_one_user_message() {
        let m = vec![
            message("system", "Be brief.".into()),
            message("user", "ox".into()),
        ];
        let out = prompted(&m, &schema());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].role, "user");
        let text = &out[0].content;
        assert!(
            text.starts_with("Be brief.\n\nox\n\nAnswer format:"),
            "{text}"
        );
        assert!(text.contains("these keys and fill in every one: \"word\"."));
        assert!(text.ends_with("{\"word\":\"...\"}"), "{text}");
        let lone = prompted(&[message("system", "S".into())], &schema());
        assert_eq!(lone.len(), 1);
        assert!(lone[0].content.starts_with("S\n\nAnswer format:"));
    }

    #[test]
    fn memory_survives_a_reopen_and_forgets() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("modes.json");
        let memory = ModeMemory::open(&path);
        assert_eq!(memory.get("u", "m"), None);
        memory.remember("u", "m", JsonMode::Prompted);
        let again = ModeMemory::open(&path);
        assert_eq!(again.get("u", "m"), Some(JsonMode::Prompted));
        again.forget("u", "m");
        assert_eq!(ModeMemory::open(&path).get("u", "m"), None);
        std::fs::write(&path, "not json").unwrap();
        assert_eq!(ModeMemory::open(&path).get("u", "m"), None);
    }
}
