//! HTTP dispatch, retry semantics, and (optionally) SSE streaming.

pub mod dispatcher;
pub mod models;
#[cfg(feature = "stream")]
pub mod stream;
