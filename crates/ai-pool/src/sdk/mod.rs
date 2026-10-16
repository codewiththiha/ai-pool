//! A request builder; schema property descriptions act as field prompts.

mod builder;
pub mod strict;

pub use builder::{ChatBuilder, ReasoningEffort, ResponseFormat, ThinkingConfig, Unstructured};
