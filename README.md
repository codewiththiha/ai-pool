# ai-pool

A fault-tolerant Rust client for OpenAI-compatible APIs: key pooling,
rotation, per-key rate quotas, and encrypted local key storage.

Hand the pool a request; it picks a healthy key, retries where retrying
helps, rotates where it doesn't, and tells you precisely why when nothing
works. It speaks the OpenAI chat-completions dialect, so one client type
covers OpenAI, Gemini, OpenRouter, local servers and anything else that
dialect reaches.

## Features

- **Pooling and rotation**: round-robin across keys, or sticky failover.
- **Failure handling**: rate limits cool a key down, dead keys are banned,
  server errors retry with backoff, request errors fail fast.
- **Per-key quotas**: per-minute and per-hour limits and concurrency caps,
  with live usage snapshots for dashboards.
- **Encrypted storage**: AES-256-GCM sealed keys in one SQLite file, scoped
  per provider; the master key lives in the OS keychain, a file, or memory.
- **Structured output** from any JSON Schema, strict-mode normalised; a
  derive path is optional.
- **Adaptive JSON** for models that refuse or ignore schemas: JSON mode,
  then a strict chat prompt, remembering what works per model.
- **Keyless mode** for Ollama, LM Studio and other local servers.
- **Streaming** that survives characters split across network frames and
  errors injected mid-stream.

## Installation

```toml
[dependencies]
ai-pool = { git = "https://github.com/codewiththiha/ai-pool", features = ["sqlite", "os-keychain", "stream"] }
```

| Feature | Enables | Extra dependencies |
|---|---|---|
| `derive` (default) | `response_format_schema::<T>()` from `#[derive(JsonSchema)]` | `schemars` |
| `sqlite` | Encrypted, scoped key store in one file | `rusqlite` (bundled SQLite) |
| `os-keychain` | Master key in the macOS Keychain, Windows Credential Manager or Secret Service | `keyring` |
| `stream` | SSE streaming (`chat_stream`, `send_stream`) | `futures-util` |

On Linux the keychain uses the Secret Service over a pure-Rust D-Bus client,
so no system libraries are needed and keys survive reboots.

## Quick start

```rust,ignore
use ai_pool::AiClient;

let client = AiClient::builder()
    .default_url("https://api.openai.com/v1/")
    .api_keys(["sk-first-key", "sk-second-key"])
    .build()
    .await?;

let text = client
    .chat_builder()
    .model("gpt-4o-mini")
    .user("Hello!")
    .send_text()
    .await?;
```

## Providers

One client per provider; `default_url` ends where `/chat/completions`
would start.

| Provider | `default_url` | Auth |
|---|---|---|
| OpenAI | `https://api.openai.com/v1/` | default (`Bearer`) |
| Gemini | `https://generativelanguage.googleapis.com/v1beta/openai/` | default |
| OpenRouter | `https://openrouter.ai/api/v1/` | default; add `.header("X-Title", "My App")` |
| Ollama | `http://localhost:11434/v1/` | `.keyless()` |
| LM Studio | `http://localhost:1234/v1/` | `.keyless()` |

Providers that want the key in another header use
`.auth(Auth::Header("api-key".into()))`. `client.models().await?` lists the
models an endpoint offers, sorted, with Gemini's `models/` prefix removed:
enough to fill a model picker.

## How failures are handled

| Response | Action |
|---|---|
| 2xx | Return the response |
| 429 | Cool the key down (honouring `Retry-After`), rotate, retry |
| 429 `insufficient_quota` | Ban the key: waiting will not refill it |
| 401 / 402 / 403 | Ban the key, rotate, retry |
| 400 naming an invalid key | Ban the key, rotate, retry (Gemini answers bad keys this way) |
| 400 / 404 / 413 / 422 | Fail at once with `BadRequest`; the request itself is wrong |
| 408 / 5xx | Retry the same key with exponential backoff and jitter |
| Timeout | Fail at once; the key is not at fault |

`Retry-After` may be seconds or an HTTP date; cooldowns are capped at an
hour. When no key is usable, requests fail with
`AiError::AllKeysExhausted { retry_in_ms }`: the soonest recovery, or `None`
when only a person can fix it (every key banned, or none added).
`AiError::is_retryable()` sorts errors worth retrying later from the rest.

Banned keys carry their reason. `manager().recover_key(id)` reinstates one.

## Rotation modes

**Proactive** (default) advances a shared cursor per request, spreading
concurrent load evenly.

**Reactive** stays on one key until it fails or runs out of quota. With a
concurrency cap, overflow spills to the next key at once instead of queueing
behind a rate limit, and the primary keeps its role when it has room again.

## Per-key quotas

Every limit is optional and per key:

```rust,ignore
use ai_pool::{AiClient, KeyLimits, RotationMode};

let client = AiClient::builder()
    .rotation_mode(RotationMode::Reactive)
    .api_key_with_limits("sk-limited", KeyLimits::none().per_minute(5).per_hour(100))
    .api_key("sk-other")
    .build()
    .await?;

let quota = client.manager().key_quota(&key_id)?;
```

Windows start at a key's first use; a key that reaches its limit sits out
until the window ends. Hour windows persist with the `sqlite` store.
Concurrency caps need reactive mode; combining them with proactive mode is
rejected at build time.

## Key management

```rust,ignore
let manager = client.manager();
let keys = manager.list_keys().await?;     // censored: "sk-proj...8f92"
let id = manager.add_key("sk-new-key\n").await?; // trimmed; blank keys refused
manager.ban_key(&id).await?;
manager.recover_key(&id).await?;
manager.set_key_limits(&id, Some(KeyLimits::none().per_minute(10))).await?;
let secret = manager.get_decrypted_key(&id).await?; // explicit, on demand
manager.remove_key(&id).await?;
```

Nothing `list_keys` returns contains plaintext.

## Encrypted persistence

```rust,ignore
use ai_pool::{AiClient, MasterKeyProvider};

let gemini = AiClient::builder()
    .default_url("https://generativelanguage.googleapis.com/v1beta/openai/")
    .sqlite_store(data_dir.join("keys.db"))
    .key_scope("gemini")
    .master_key_provider(MasterKeyProvider::os_keychain("com.example.myapp"))
    .build()
    .await?;
```

- **Scopes** let one file hold every provider's keys without mixing them.
- **Master keys**: `os_keychain(service)` under your app's own service name,
  `File(path)` (created owner-only), `custom_from_slice(&bytes)`, or
  `Ephemeral` (the default, for the in-memory store). Resolving may block
  or prompt, so `build` does it off the async workers.
- **Tampering**: a modified row fails authentication; that key is banned
  as corrupted and the rest load normally.
- **Lost master key**: if the key is gone (keychain reset, file deleted),
  a new one is created and old rows are banned with `LOST_MASTER_KEY`.
  Adding the same key again replaces the row, so users just re-enter it.
- **Upgrades**: rows from 0.2's unscoped table move to the empty scope.

Seeded keys get ids derived from their contents, so re-running the builder
never duplicates rows.

## Structured output

Any JSON Schema works, so a schema written once can serve this crate and
any other backend:

```rust,ignore
#[derive(serde::Deserialize)]
struct Sentiment { label: String, confidence: f64 }

let schema = serde_json::json!({
    "type": "object",
    "properties": {
        "label": { "type": "string", "enum": ["positive", "negative", "neutral"] },
        "confidence": { "type": "number", "description": "Between 0 and 1." }
    },
    "required": ["label", "confidence"]
});

let result: Sentiment = client
    .chat_builder()
    .model("gpt-4o-mini")
    .user("The product arrived two days early and works great.")
    .response_json_schema("sentiment", schema)
    .send()
    .await?;
```

- Strict mode needs every object closed and every property required, so
  the schema is rewritten on the way out: `additionalProperties: false`
  everywhere, optional properties become required and nullable. Model
  optional fields as `Option<T>`. `ResponseFormat::JsonSchema { strict:
  false, .. }` sends the schema untouched.
- Property order is preserved; models fill fields in that order.
- `.response_json::<T>()` asks for any JSON object, for servers without
  schema support; describe the shape in the prompt.
- Answers wrapped in code fences or prose still parse.
- A refusal surfaces as `AiError::Refused(reason)`; an answer cut off at
  the token limit as `AiError::Truncated`.
- With `derive`, `.response_format_schema::<T>("name")` builds the schema
  from `#[derive(JsonSchema)]`, field doc comments becoming descriptions.

### Any model, adaptively

Servers differ in what they honour. Some refuse `json_schema` with a 400,
some accept it and ignore it (Ollama's cloud models), and some only chat.
`.structured(name, schema)` asks in the strictest way the model honours.
It steps down when a mode is refused, or when the answer misses the
schema:

| Mode | Request |
|---|---|
| `JsonMode::Schema` | `json_schema` format, the schema also in the system text |
| `JsonMode::Object` | `json_object` format, the schema in the system text |
| `JsonMode::Prompted` | no format; one user message holding the instructions, the request and strict answer rules: one JSON object, no markdown or commentary, these keys, this schema, this example shape |

```rust,ignore
let answer = client
    .chat_builder()
    .model("gemma3")
    .system("Explain the word in its context.")
    .user("Word: \"quick\"")
    .structured("word_info", schema)
    .send()
    .await?;
println!("{} via {:?}", answer.value, answer.mode);
```

- **It sticks.** The mode that worked is remembered per base URL and
  model, so the next request starts there; another model on the same
  server starts again from `Schema`. `ModeMemory::open(path)` keeps the
  memory in a JSON file across runs, `.json_modes(memory)` shares it
  between clients, and `forget(url, model)` starts a model over.
- **Answers are read leniently.** Fences, prose around the object and a
  leading `<think>` block are skipped. Checking covers types, required
  keys and enums; extra keys and lengths are allowed.
- **Even the last mode answers.** If the answer still misses the schema,
  `send` returns it with `mismatch` saying what is off. Text with no JSON
  at all is `AiError::InvalidResponse`. Any other failure (a bad key, a
  missing model, a timeout) is returned at once, without stepping down.
- **Streaming.** With `stream`, `.send_stream()` yields
  `StructuredEvent::Partial` values as the object fills in, `Retry(mode)`
  when it steps down (partials start over), then `Done(answer)`.
- `ai_pool::json` has the pieces: `extract`, `partial` (closes an
  unfinished object), `check` and `skeleton`.

Reasoning controls map to the JSON providers expect:

| Configuration | Payload |
|---|---|
| `ThinkingConfig::Effort(ReasoningEffort::High)` | `"reasoning_effort": "high"` |
| `ThinkingConfig::Effort(ReasoningEffort::None)` | `"reasoning_effort": "none"` (Gemini 2.5 Flash, GPT-5.1) |
| `ThinkingConfig::Enabled { budget_tokens: Some(1024) }` | `"thinking": {"thinking_budget": 1024}` |
| `ThinkingConfig::Disabled` | `"reasoning_effort": null` |

`.max_completion_tokens(n)` sends the field OpenAI reasoning models require
instead of `max_tokens`. Anything else goes through `.extra(key, value)`.

Content arriving as an array of parts is joined into text, non-text parts
skipped.

## Streaming

```rust,ignore
use futures_util::StreamExt;

let mut stream = client.chat_builder().model("gpt-4o-mini").user("A poem").send_stream().await?;
while let Some(chunk) = stream.next().await {
    if let Some(text) = chunk?.choices.first().and_then(|c| c.delta.content.as_deref()) {
        print!("{text}");
    }
}
```

An error event inside a live stream updates the key's health and ends the
stream with `AiError::StreamInterrupted`.

## Custom storage

Implement `KeyStore` and pass it with `.custom_store(...)`.

## Minimum supported Rust version

Rust 1.88 (edition 2024).

## Contributing

Read [AGENTS.md](AGENTS.md) first: comment and commit rules, the never-crash
rules, and the gates CI runs.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.
