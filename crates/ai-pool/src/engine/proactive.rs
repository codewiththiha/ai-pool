//! Proactive rotation: lock-free atomic round-robin over the usable subset.

use std::sync::atomic::{AtomicUsize, Ordering};

/// Advances the shared cursor over the usable keys.
pub(crate) fn pick(cursor: &AtomicUsize, usable: &[(usize, String)]) -> String {
    let idx = cursor.fetch_add(1, Ordering::Relaxed) % usable.len();
    usable[idx].1.clone()
}
