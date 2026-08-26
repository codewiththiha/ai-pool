//! Proactive rotation: lock-free atomic round-robin over the usable subset.

use std::sync::atomic::{AtomicUsize, Ordering};

/// Advances the shared cursor and maps it onto the usable key list.
///
/// The cursor is global and monotonically increasing, so concurrent callers
/// naturally spread across the pool (`index % usable.len()`).
pub(crate) fn pick(cursor: &AtomicUsize, usable: &[(usize, String)]) -> String {
    let idx = cursor.fetch_add(1, Ordering::Relaxed) % usable.len();
    usable[idx].1.clone()
}
