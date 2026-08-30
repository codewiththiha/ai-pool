# ai-pool

A fault-tolerant Rust client for OpenAI-compatible APIs with key pooling,
automatic rotation, per-key rate quotas, and encrypted local key storage.

`ai-pool` is for applications that hold a pool of API keys (often supplied by
end users) and need requests to keep flowing when individual keys get rate
limited, revoked, or the upstream has a bad day. Instead of handling those
failures at every call site, you hand the pool a request and it picks a
healthy key, retries where retrying makes sense, and rotates where it
doesn't.

## Features

- **Key pooling and rotation** — round-robin across all keys, or sticky
  failover that only moves off a key when it stops working.
- **Sensible failure handling** — rate limits put a key on cooldown and
  rotate, auth failures ban the key and rotate, server errors retry the same
  key with backoff, and client errors fail fast without burning other keys.
- **Per-key rate quotas** — optional per-minute and per-hour request limits
  and per-key concurrency caps, tracked individually per key, with live
  usage snapshots for building dashboards or countdowns.
- **Encrypted storage** — keys are sealed with AES-256-GCM envelope
  encryption before they touch disk. The master key can live in the OS
  keychain, come from your own source, or stay in memory.
- **Typed request builder** — build chat requests fluently, request strict
  JSON-schema output derived from your own structs, and get replies
  deserialized into them.
- **Streaming** — SSE streaming with mid-stream error detection: a rate
  limit injected into a live stream is caught, the key is cooled down, and
  the stream ends with a typed error.

## Installation

```toml
[dependencies]
ai-pool = { version = "0.2", features = ["sqlite", "os-keychain", "stream"] }
```

All features are optional. The default build keeps keys in memory and pulls
in no database or keychain dependencies.

| Feature | Enables | Extra dependencies |
|---|---|---|
| `sqlite` | Encrypted persistent key store | `sqlx` |
| `os-keychain` | Master key in the OS keychain | `keyring` |
| `stream` | SSE streaming (`chat_stream`) | `futures-util` |

## Quick start

```rust
use ai_pool::{AiClient, RotationMode};
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<(), ai_pool::AiError> {
    let client = AiClient::builder()
        .default_url("https://api.openai.com/v1/")
        .rotation_mode(RotationMode::Proactive)
        .request_timeout(Duration::from_secs(60))
        .api_keys(vec!["sk-first-key", "sk-second-key"])
        .build()
        .await?;

    let resp = client.chat(serde_json::json!({
        "model": "gpt-4o-mini",
        "messages": [{"role": "user", "content": "Hello!"}]
    })).await?;

    println!("{}", resp.choices[0].message.content);
    Ok(())
}
```

Any OpenAI-compatible endpoint works: point `default_url` at OpenAI, Gemini's
OpenAI-compatible endpoint, OpenRouter, a local server, and so on.

## How failures are handled

The retry policy is fixed and applies to every request:

| Response | Action |
|---|---|
| 2xx | Return the response |
| 429 | Put the key on cooldown (honoring `Retry-After`), rotate, retry |
| 401 / 403 | Ban the key, rotate, retry |
| 400 / 404 / 422 | Fail immediately; the request itself is wrong |
| 5xx | Retry the same key with exponential backoff and jitter |
| Timeout | Fail immediately; the key is not at fault |

`Retry-After` accepts both wire forms defined by RFC 7231 — a number of
seconds or an absolute `IMF-fixdate` (e.g. `Wed, 21 Oct 2015 07:28:00 GMT`) —
and any resulting cooldown is clamped to a 1-hour ceiling so an odd value
never takes a key offline unreasonably long.

When every key is on cooldown, out of quota, or banned, requests fail with
`AiError::AllKeysExhausted { retry_in_ms }`, where `retry_in_ms` is the
soonest known recovery time (`None` if recovery time is unknowable, for
example when every key is banned).

## Rotation modes

**Proactive** (default) advances an atomic cursor on every request, so
concurrent load spreads evenly across all healthy keys.

**Reactive** sticks to one key until it fails or runs out of quota, then
fails over to the next. If a key has a concurrency cap, overflow requests
spill to the next key immediately instead of queueing behind a rate limit
error, and the primary key keeps its role once it has capacity again.

## Per-key quotas

Every limit is optional and per key. A key can carry any combination, or
none:

```rust
use ai_pool::{AiClient, KeyLimits, RotationMode};

let client = AiClient::builder()
    .default_url("https://api.openai.com/v1/")
    .rotation_mode(RotationMode::Reactive)
    // This key allows 5 requests per minute, 100 per hour.
    .api_key_with_limits(
        "sk-limited-key",
        KeyLimits::none().per_minute(5).per_hour(100),
    )
    // This key inherits the pool default (unlimited here).
    .api_key("sk-other-key")
    .build()
    .await?;
```

Windows start counting at a key's first use. When a key reaches its limit it
sits out until the window ends, then recovers on its own; other keys absorb
the traffic in the meantime. Hour windows are persisted (with the `sqlite`
store), so usage carries across restarts while the window is still current
and resets once it has passed.

Live usage is available per key:

```rust
let quota = client.manager().key_quota(&key_id)?;
if let Some(minute) = quota.minute {
    println!(
        "{} of {:?} used, resets in {}ms",
        minute.used, minute.limit, minute.resets_in_ms
    );
}
```

The same snapshot is embedded in `list_keys()`, which is enough to drive a
usage dashboard with realtime countdowns.

Concurrency caps (`max_concurrency`) only apply in reactive mode, since
proactive rotation already spreads load; combining them with proactive mode
is rejected at build time.

## Key management

`client.manager()` exposes the administrative surface:

```rust
let manager = client.manager();

// Censored listing: ids, "sk-proj...8f92" display text, health, quota.
let keys = manager.list_keys().await?;

// Add, remove, ban, recover.
let id = manager.add_key("sk-new-key").await?;
manager.ban_key(&id).await?;
manager.recover_key(&id).await?;
manager.remove_key(&id).await?;

// Change limits at runtime.
manager.set_key_limits(&id, Some(KeyLimits::none().per_minute(10))).await?;

// Explicit decryption, only when plaintext is genuinely needed.
use ai_pool::ExposeSecret;
let secret = manager.get_decrypted_key(&id).await?;
let plaintext = secret.expose_secret();
```

Nothing returned by `list_keys` contains plaintext, so it can be handed to
UI code or logged without leaking secrets.

## Encrypted persistence

With the `sqlite` feature, keys are stored encrypted at rest:

```rust
use ai_pool::MasterKeyProvider;

let client = AiClient::builder()
    .default_url("https://api.openai.com/v1/")
    .sqlite_store("data/keys.db")
    .master_key_provider(MasterKeyProvider::OsKeychain) // feature: os-keychain
    .api_keys(vec!["sk-my-key"])
    .build()
    .await?;
```

Each key is sealed with AES-256-GCM under a random nonce before it is
written. Because the encryption is authenticated, a modified database row
fails decryption cleanly: that key is marked banned as corrupted and the
rest of the pool loads normally. Seeded keys get deterministic ids derived
from their contents, so re-running the builder after a restart never creates
duplicate rows.

The master key can also be supplied directly
(`MasterKeyProvider::custom_from_slice(&bytes)`) or generated fresh per run
(`MasterKeyProvider::Ephemeral`, the default, suited to the in-memory
store).

## Typed requests and structured output

`chat_builder()` assembles requests without hand-written JSON:

```rust
use ai_pool::{JsonSchema, ReasoningEffort, ThinkingConfig};
use serde::Deserialize;

// Doc comments become schema descriptions the model sees per field.
#[derive(Debug, Deserialize, JsonSchema)]
struct Sentiment {
    /// Exactly "positive", "negative", or "neutral".
    label: String,
    /// Confidence between 0.0 and 1.0.
    confidence: f64,
}

let result: Sentiment = client
    .chat_builder()
    .model("gpt-4o-mini")
    .system("You are a sentiment classifier.")
    .user("The product arrived two days early and works great.")
    .temperature(0.2)
    .thinking(ThinkingConfig::Effort(ReasoningEffort::High))
    .response_format_schema::<Sentiment>("sentiment")
    .send()
    .await?;
```

Calling `response_format_schema::<T>()` generates a strict JSON schema from
`T`, attaches it to the request, and types the builder so that `send()`
returns `T` directly. Replies wrapped in markdown code fences are handled.
Without a schema, `send()` returns the raw response and `send_text()`
returns the first choice's text.

Reasoning controls map to the JSON different providers expect:

| Configuration | Payload |
|---|---|
| `ThinkingConfig::Effort(ReasoningEffort::High)` | `"reasoning_effort": "high"` |
| `ThinkingConfig::Enabled { budget_tokens: Some(1024) }` | `"thinking": {"thinking_budget": 1024}` |
| `ThinkingConfig::Disabled` | `"reasoning_effort": null` |

Provider-specific fields can be added with `.extra(key, value)`.

Responses are tolerated in both wire forms some OpenAI-compatible providers
emit: `message.content` may be a plain string, as OpenAI sends it, or an array
of content parts (`[{"type":"text","text":"..."}]`), as Gemini and several
aggregators send it — the latter is normalized to the joined text, with
non-text parts (images/audio) skipped. `send_text()` and structured `send()`
work unchanged either way.

## Streaming

With the `stream` feature:

```rust
use futures_util::StreamExt;

let mut stream = client
    .chat_builder()
    .model("gpt-4o-mini")
    .user("Write a short poem about the sea.")
    .send_stream()
    .await?;

while let Some(chunk) = stream.next().await {
    let chunk = chunk?;
    if let Some(text) = chunk.choices[0].delta.content.as_deref() {
        print!("{text}");
    }
}
```

Some providers return 200, start streaming, and then inject an error object
mid-stream. The parser detects this, updates the key's health (cooldown for
rate limits, ban for auth errors), and ends the stream with
`AiError::StreamInterrupted`.

## Custom storage

Storage is behind the `KeyStore` trait. Implement it to back the pool with
anything else and pass it via `.custom_store(...)`; the built-in options are
the in-memory store (default) and the SQLite store.

## Minimum supported Rust version

Rust 1.85 (edition 2024).

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.
