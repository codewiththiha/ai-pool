//! Reactive rotation: stay on a primary key until it fails or saturates.

use std::sync::RwLock;

/// Keeps the primary while usable, else promotes the first usable key.
pub(crate) fn pick(
    primary: &RwLock<Option<String>>,
    usable: &[(usize, String)],
    is_saturated: impl Fn(&str) -> bool,
) -> Option<String> {
    // Fast path: current primary is still usable and has capacity.
    {
        let guard = primary
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(current) = guard.as_deref()
            && usable.iter().any(|(_, id)| id == current)
            && !is_saturated(current)
        {
            return Some(current.to_string());
        }
    }

    let current = primary
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    let primary_still_usable = current
        .as_deref()
        .is_some_and(|c| usable.iter().any(|(_, id)| id == c));

    // First usable, non-saturated key in stable order.
    let next = usable
        .iter()
        .map(|(_, id)| id)
        .find(|id| !is_saturated(id))?
        .clone();

    // Promote only when the old primary is unusable, not merely busy.
    if !primary_still_usable {
        *primary
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(next.clone());
    }
    Some(next)
}
