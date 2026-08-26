//! The main `AiClient` struct and its builder.

use std::sync::Arc;
use std::time::Duration;

use secrecy::SecretString;

use crate::config::{PoolConfig, RotationMode};
use crate::crypto::{Envelope, MasterKeyProvider};
use crate::engine::{KeyPool, KeySeed};
use crate::error::AiError;
use crate::http::dispatcher::Dispatcher;
use crate::http::models::ChatResponse;
use crate::quota::KeyLimits;
use crate::storage::{memory::MemoryStore, KeyManager, KeyStore, Vault};

#[cfg(feature = "stream")]
use crate::http::stream::ChatStream;

/// Which storage backend the builder should construct.
enum StoreChoice {
    Memory,
    #[cfg(feature = "sqlite")]
    Sqlite(std::path::PathBuf),
    Custom(Arc<dyn KeyStore>),
}

/// Fault-tolerant, key-pooling AI API client.
///
/// Cheap to clone (the internals are behind `Arc`s), so it can be shared
/// freely across tasks or stored in application state.
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

    /// Non-streaming OpenAI-compatible chat completion. Picks a healthy key
    /// from the pool, injects it, and applies the full retry/rotation policy.
    pub async fn chat(&self, payload: serde_json::Value) -> Result<ChatResponse, AiError> {
        self.dispatcher.chat(payload).await
    }

    /// Streaming chat completion (SSE). Requires the `stream` feature.
    #[cfg(feature = "stream")]
    pub async fn chat_stream(&self, payload: serde_json::Value) -> Result<ChatStream, AiError> {
        self.dispatcher.chat_stream(payload).await
    }

    /// Developer control surface: CRUD, censored listings, explicit
    /// decryption, ban/recover.
    pub const fn manager(&self) -> &KeyManager {
        &self.manager
    }

    /// Starts a fluent, type-safe request builder, as an alternative to
    /// passing raw JSON to [`Self::chat`].
    ///
    /// ```no_run
    /// # use ai_pool::{AiClient, ThinkingConfig, ReasoningEffort};
    /// # async fn run(client: &AiClient) -> Result<(), ai_pool::AiError> {
    /// let text = client
    ///     .chat_builder()
    ///     .model("gemini-2.0-flash")
    ///     .user("hello")
    ///     .thinking(ThinkingConfig::Effort(ReasoningEffort::High))
    ///     .send_text()
    ///     .await?;
    /// # Ok(())
    /// # }
    /// ```
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
    master: MasterKeyProvider,
    seed_keys: Vec<(String, Option<KeyLimits>)>,
}

impl Default for AiClientBuilder {
    fn default() -> Self {
        Self {
            config: PoolConfig::default(),
            store: StoreChoice::Memory,
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

    /// Use the encrypted SQLite store at `path`. Requires feature `sqlite`.
    #[cfg(feature = "sqlite")]
    pub fn sqlite_store(mut self, path: impl Into<std::path::PathBuf>) -> Self {
        self.store = StoreChoice::Sqlite(path.into());
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

    /// Seed API keys. Ids are deterministic hashes of the plaintext, so
    /// re-running the builder after an app restart never creates duplicates.
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

    /// Seeds a key with its own quota limits (overrides the pool default).
    ///
    /// ```no_run
    /// # use ai_pool::{AiClient, KeyLimits};
    /// # let b = AiClient::builder();
    /// b.api_key_with_limits(
    ///     "sk-...",
    ///     KeyLimits::none().per_minute(5).per_hour(100),
    /// );
    /// ```
    pub fn api_key_with_limits(
        mut self,
        key: impl Into<String>,
        limits: KeyLimits,
    ) -> Self {
        self.seed_keys.push((key.into(), Some(limits)));
        self
    }

    /// Default quota limits for every key that has no per-key limits.
    /// Each key still tracks its own windows independently.
    pub const fn default_key_limits(mut self, limits: KeyLimits) -> Self {
        self.config.default_limits = limits;
        self
    }

    /// Resolves the master key, opens the store, seeds keys idempotently,
    /// decrypts everything into RAM, and wires up the dispatcher.
    ///
    /// Fails with [`AiError::InvalidConfig`] when a `max_concurrency` limit
    /// is combined with [`RotationMode::Proactive`]: proactive round-robin
    /// already spreads load, so per-key concurrency splitting only applies
    /// to reactive mode.
    pub async fn build(self) -> Result<AiClient, AiError> {
        // 0. Config validation: concurrency limits are reactive-only.
        if self.config.rotation_mode == RotationMode::Proactive {
            let conflicting = self.config.default_limits.max_concurrency.is_some()
                || self
                    .seed_keys
                    .iter()
                    .any(|(_, l)| l.is_some_and(|l| l.max_concurrency.is_some()));
            if conflicting {
                return Err(AiError::InvalidConfig(
                    "max_concurrency limits require RotationMode::Reactive;                      proactive mode already rotates keys per request"
                        .into(),
                ));
            }
        }

        // 1. Master key (may hit the OS keychain → VaultError::KeychainLocked).
        let master = self.master.resolve()?;
        let envelope = Envelope::new(master);

        // 2. Storage backend.
        #[cfg(feature = "sqlite")]
        let mut sqlite_pool_handle: Option<sqlx::SqlitePool> = None;

        let store: Arc<dyn KeyStore> = match self.store {
            StoreChoice::Memory => Arc::new(MemoryStore::new()),
            StoreChoice::Custom(store) => store,
            #[cfg(feature = "sqlite")]
            StoreChoice::Sqlite(path) => {
                let s = crate::storage::sqlite::SqliteStore::open(&path).await?;
                sqlite_pool_handle = Some(s.pool().clone());
                Arc::new(s)
            }
        };
        let vault = Vault::new(store, envelope);

        // 3. Idempotent seeding (INSERT OR IGNORE keeps existing rows and
        //    their persisted quota windows).
        for (plaintext, limits) in &self.seed_keys {
            let id = crate::crypto::deterministic_id(plaintext);
            let secret = SecretString::from(plaintext.clone());
            vault.insert_plain(&id, &secret, *limits).await?;
        }

        // 4. Decrypt everything into the live pool. Tampered rows are banned
        //    and skipped without crashing; persisted hour windows are summed
        //    up if still current, reset if their time passed.
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

        // Re-validate against persisted limits loaded from the vault.
        if self.config.rotation_mode == RotationMode::Proactive
            && pool.any_concurrency_limit()
        {
            return Err(AiError::InvalidConfig(
                "a persisted key carries max_concurrency, which requires                  RotationMode::Reactive"
                    .into(),
            ));
        }

        #[cfg(feature = "sqlite")]
        if let Some(p) = sqlite_pool_handle {
            pool.register_sqlite_pool(p);
        }

        // 5. Dispatcher + manager.
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
