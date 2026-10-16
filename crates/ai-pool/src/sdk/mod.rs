//! A request builder; schema property descriptions act as field prompts.

mod adaptive;
mod builder;
pub mod json;
pub mod strict;

#[cfg(feature = "stream")]
pub use adaptive::StructuredStream;
pub use adaptive::{
    JsonMode, ModeMemory, Structured, StructuredAnswer, StructuredEvent, format_refused,
};
pub use builder::{ChatBuilder, ReasoningEffort, ResponseFormat, ThinkingConfig, Unstructured};
