#![doc = include_str!("../../../README.md")]

pub mod client;
pub mod config;
pub mod crypto;
pub mod engine;
pub mod error;
pub mod http;
pub mod quota;
pub mod sdk;
pub mod storage;

pub use client::{AiClient, AiClientBuilder};
pub use config::{Auth, PoolConfig, RotationMode};
pub use crypto::{MasterKeyProvider, ResolvedKey};
pub use engine::KeySeed;
pub use error::{AiError, VaultError};
pub use http::models::{ChatResponse, Choice, Message, ModelInfo, Usage};
pub use quota::{KeyLimits, KeyQuotaInfo, WindowInfo, WindowState};
#[cfg(feature = "stream")]
pub use sdk::StructuredStream;
pub use sdk::{
    ChatBuilder, JsonMode, ModeMemory, ReasoningEffort, ResponseFormat, Structured,
    StructuredAnswer, StructuredEvent, ThinkingConfig, Unstructured, json,
};
pub use storage::{
    KeyHealth, KeyManager, KeyMetadata, KeyRecord, KeyStatus, KeyStore, LOST_MASTER_KEY,
};

#[cfg(feature = "stream")]
pub use error::StreamInterruption;
#[cfg(feature = "stream")]
pub use http::models::{ChatChunk, ChunkChoice, Delta};
#[cfg(feature = "stream")]
pub use http::stream::ChatStream;

pub use secrecy::{ExposeSecret, SecretBox, SecretString};

#[cfg(feature = "derive")]
pub use schemars::{self, JsonSchema};
