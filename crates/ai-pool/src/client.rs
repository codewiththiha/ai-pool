//! The main `AiClient` struct and its builder.

use std::sync::Arc;
use std::time::Duration;

use secrecy::SecretString;

use crate::config::{Auth, PoolConfig, RotationMode};
use crate::crypto::{Envelope, MasterKeyProvider};
use crate::engine::{KeyPool, KeySeed};
use crate::error::AiError;
use crate::http::dispatcher::Dispatcher;
use crate::http::models::{ChatResponse, ModelInfo};
use crate::quota::KeyLimits;
use crate::storage::{KeyManager, KeyStore, Vault, memory::MemoryStore};

#[cfg(feature = "stream")]
use crate::http::stream::ChatStream;

/// Which storage backend the builder should construct.
enum StoreChoice {
    Memory,
    #[cfg(feature = "sqlite")]
    Sqlite(std::path::PathBuf),
    Custom(Arc<dyn KeyStore>),
}

/// A key-pooling client for OpenAI-compatible APIs; cheap to clone.
#[derive(Clone)]
pub struct AiClient {
    dispatcher: Arc<Dispatcher>,
    manager: KeyManager,
}

impl std::fmt::Debug for AiClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AiClient").finish_non_exhaustive()
    }
}

impl AiClient {
    /// Entry point: `AiClient::builder()...build().await`.
    pub fn builder() -> AiClientBuilder {
        AiClientBuilder::default()
    }

    /// Chat completion through the pool, with rotation and retries.
    pub async fn chat(&self, payload: serde_json::Value) -> Result<ChatResponse, AiError> {
        self.dispatcher.chat(payload).await
    }

    /// Streaming chat completion (SSE). Requires the `stream` feature.
    #[cfg(feature = "stream")]
    pub async fn chat_stream(&self, payload: serde_json::Value) -> Result<ChatStream, AiError> {
        self.dispatcher.chat_stream(payload).await
    }

    /// The models the endpoint offers, e.g. to fill a picker.
    pub async fn models(&self) -> Result<Vec<ModelInfo>, AiError> {
        self.dispatcher.models().await
    }

    /// Key management: add, list censored, decrypt, ban, recover.
    pub const fn manager(&self) -> &KeyManager {
        &self.manager
    }

    /// A typed request builder instead of raw JSON.
    pub fn chat_builder(&self) -> crate::sdk::ChatBuilder<'_> {
        crate::sdk::ChatBuilder::new(self)
    }
}

/// Builder for [`AiClient`]. Safe defaults: in-memory store + ephemeral
/// master key, proactive rotation, 60s timeout.
#[must_use = "call .build().await to construct the client"]
pub struct AiClientBuilder {
    config: PoolConfig,
    store: StoreChoice,
    scope: String,
    master: MasterKeyProvider,
    seed_keys: Vec<(String, Option<KeyLimits>)>,
}

impl Default for AiClientBuilder {
    fn default() -> Self {
        Self {
            config: PoolConfig::default(),
            store: StoreChoice::Memory,
            scope: String::new(),
            master: MasterKeyProvider::Ephemeral,
            seed_keys: Vec::new(),
        }
    }
}

impl AiClientBuilder {
    /// Base URL of the OpenAI-compatible API.
    pub fn default_url(mut self, url: impl Into<String>) -> Self {
        self.config.default_url = url.into();
        self
    }

    /// How keys are sent; [`Auth::None`] needs no keys at all.
    pub fn auth(mut self, auth: Auth) -> Self {
        self.config.auth = auth;
        self
    }

    /// Shorthand for [`Auth::None`]: local servers like Ollama.
    pub fn keyless(self) -> Self {
        self.auth(Auth::None)
    }

    /// Adds a header to every request, e.g. `OpenRouter`'s `X-Title`.
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.config.headers.push((name.into(), value.into()));
        self
    }

    /// Deadline for establishing a connection (default 10 s).
    pub const fn connect_timeout(mut self, timeout: Duration) -> Self {
        self.config.connect_timeout = timeout;
        self
    }

    /// Rotation strategy: proactive round-robin or reactive sticky failover.
    pub const fn rotation_mode(mut self, mode: RotationMode) -> Self {
        self.config.rotation_mode = mode;
        self
    }

    /// Semaphore limit on concurrent in-flight requests.
    pub fn concurrency_limit(mut self, limit: usize) -> Self {
        self.config.concurrency_limit = limit.max(1);
        self
    }

    /// Per-request timeout.
    pub const fn request_timeout(mut self, timeout: Duration) -> Self {
        self.config.request_timeout = timeout;
        self
    }

    /// How many times a 5xx is retried on the same key (default 3).
    pub const fn max_server_error_retries(mut self, retries: u32) -> Self {
        self.config.max_server_error_retries = retries;
        self
    }

    /// Cooldown applied on 429 when no `Retry-After` header is present.
    pub const fn default_cooldown(mut self, cooldown: Duration) -> Self {
        self.config.default_cooldown = cooldown;
        self
    }

    /// Use the in-memory store (default). Keys live in RAM only.
    pub fn memory_store(mut self) -> Self {
        self.store = StoreChoice::Memory;
        self
    }

    /// Use the encrypted `SQLite` store at `path`. Requires feature `sqlite`.
    #[cfg(feature = "sqlite")]
    pub fn sqlite_store(mut self, path: impl Into<std::path::PathBuf>) -> Self {
        self.store = StoreChoice::Sqlite(path.into());
        self
    }

    /// Separates this client's rows from other scopes in the same file.
    pub fn key_scope(mut self, scope: impl Into<String>) -> Self {
        self.scope = scope.into();
        self
    }

    /// Bring your own [`KeyStore`] implementation.
    pub fn custom_store(mut self, store: Arc<dyn KeyStore>) -> Self {
        self.store = StoreChoice::Custom(store);
        self
    }

    /// Where the 32-byte master encryption key comes from.
    pub fn master_key_provider(mut self, provider: MasterKeyProvider) -> Self {
        self.master = provider;
        self
    }

    /// Seeds keys; ids hash the plaintext, so reseeding never duplicates.
    pub fn api_keys<I, S>(mut self, keys: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.seed_keys = keys.into_iter().map(|k| (k.into(), None)).collect();
        self
    }

    /// Single-key convenience.
    pub fn api_key(mut self, key: impl Into<String>) -> Self {
        self.seed_keys.push((key.into(), None));
        self
    }

    /// Seeds a key with its own quota limits.
    pub fn api_key_with_limits(mut self, key: impl Into<String>, limits: KeyLimits) -> Self {
        self.seed_keys.push((key.into(), Some(limits)));
        self
    }

    /// Limits for keys without their own; each key counts separately.
    pub const fn default_key_limits(mut self, limits: KeyLimits) -> Self {
        self.config.default_limits = limits;
        self
    }

    /// Resolves the master key, opens the store, seeds and loads keys.
    ///
    /// `max_concurrency` limits need [`RotationMode::Reactive`].
    pub async fn build(self) -> Result<AiClient, AiError> {
        if self.config.rotation_mode == RotationMode::Proactive {
            let conflicting = self.config.default_limits.max_concurrency.is_some()
                || self
                    .seed_keys
                    .iter()
                    .any(|(_, l)| l.is_some_and(|l| l.max_concurrency.is_some()));
            if conflicting {
                return Err(AiError::InvalidConfig(
                    "max_concurrency limits require RotationMode::Reactive".into(),
                ));
            }
        }

        // The OS store may prompt or wait on D-Bus.
        let master = self.master;
        let resolved = tokio::task::spawn_blocking(move || master.resolve())
            .await
            .map_err(|e| AiError::InvalidConfig(format!("master key task: {e}")))??;
        let envelope = Envelope::new(resolved.key);

        let store: Arc<dyn KeyStore> = match self.store {
            StoreChoice::Memory => Arc::new(MemoryStore::new()),
            StoreChoice::Custom(store) => store,
            #[cfg(feature = "sqlite")]
            StoreChoice::Sqlite(path) => {
                Arc::new(crate::storage::sqlite::SqliteStore::open(&path, &self.scope).await?)
            }
        };
        let vault = Vault::new(store, envelope).fresh_master(resolved.fresh);

        // Existing rows keep their health and quota windows.
        for (plaintext, limits) in &self.seed_keys {
            let plaintext = plaintext.trim();
            if plaintext.is_empty() {
                continue;
            }
            let id = crate::crypto::deterministic_id(plaintext);
            let secret = SecretString::from(plaintext.to_string());
            vault.insert_plain(&id, &secret, *limits).await?;
        }

        let pool = Arc::new(KeyPool::new(
            self.config.rotation_mode,
            self.config.default_limits,
        ));
        for key in vault.load_all_decrypted().await? {
            pool.upsert(
                KeySeed::new(key.id, key.secret, key.censored)
                    .health(key.health)
                    .limits(key.limits)
                    .windows(key.minute_window, key.hour_window),
            );
        }

        if self.config.rotation_mode == RotationMode::Proactive && pool.any_concurrency_limit() {
            return Err(AiError::InvalidConfig(
                "a stored key has max_concurrency, which needs RotationMode::Reactive".into(),
            ));
        }

        let dispatcher = Arc::new(Dispatcher::new(
            Arc::clone(&pool),
            vault.clone(),
            self.config,
        )?);
        let manager = KeyManager::new(vault, pool);

        Ok(AiClient {
            dispatcher,
            manager,
        })
    }
}
