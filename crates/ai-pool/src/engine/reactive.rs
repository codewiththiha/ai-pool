//! Reactive rotation: sticky failover, now concurrency-aware.
//!
//! One key is the "primary" and serves every request until it becomes
//! unusable (429 cooldown / 401 ban / quota window exhausted) **or**
//! concurrency-saturated. Saturation does not demote the primary — overflow
//! merely *spills* to the next available key while the primary keeps its
//! sticky role, so traffic splits across keys instead of queueing behind a
//! rate-limit error.

use std::sync::RwLock;

/// Picks a key from `usable` (health + rate-window filtered, stable order).
///
/// `is_saturated` reports per-key concurrency saturation. Returns `None`
/// when every usable key is saturated.
pub(crate) fn pick(
    primary: &RwLock<Option<String>>,
    usable: &[(usize, String)],
    is_saturated: impl Fn(&str) -> bool,
) -> Option<String> {
    // Fast path: current primary is still usable and has capacity.
    {
        let guard = primary.read().expect("lock poisoned");
        if let Some(current) = guard.as_deref() {
            if usable.iter().any(|(_, id)| id == current) && !is_saturated(current) {
                return Some(current.to_string());
            }
        }
    }

    let current = primary.read().expect("lock poisoned").clone();
    let primary_still_usable = current
        .as_deref()
        .is_some_and(|c| usable.iter().any(|(_, id)| id == c));

    // First usable, non-saturated key in stable order.
    let next = usable
        .iter()
        .map(|(_, id)| id)
        .find(|id| !is_saturated(id))?
        .clone();

    // Only promote a new primary when the old one is truly unusable
    // (banned/cooling/out of quota). A merely saturated primary keeps its
    // role; the overflow pick is just a temporary spill.
    if !primary_still_usable {
        *primary.write().expect("lock poisoned") = Some(next.clone());
    }
    Some(next)
}
