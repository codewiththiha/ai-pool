//! Per-key quota intelligence: optional minute/hour rate windows and
//! per-key concurrency limits.
//!
//! # Window semantics
//!
//! A window starts at the key's **first use** (usage starts from 0) and lasts
//! a fixed length (60 s / 60 min). Usage counts up per leased request; when
//! it reaches the limit the key is unusable until `start + length`, at which
//! point the next use begins a fresh window. All timestamps are unix
//! milliseconds so hour windows survive persistence: a reloaded window keeps
//! summing if still current, or resets if its time has passed.
//!
//! # Live introspection
//!
//! [`WindowInfo`] / [`KeyQuotaInfo`] are serializable snapshots (used,
//! remaining, `resets_at_ms`, `resets_in_ms`) surfaced through
//! `KeyManager::list_keys`, which is enough to drive a live usage dashboard
//! or countdown timer.

use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// One minute, in milliseconds.
pub const MINUTE_MS: i64 = 60_000;
/// One hour, in milliseconds.
pub const HOUR_MS: i64 = 3_600_000;

/// Current unix time in milliseconds.
pub(crate) fn now_ms() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
    )
    .unwrap_or(i64::MAX)
}

/// Optional per-key limits. Every field is independent: a key can have any
/// combination, or none at all (unlimited).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyLimits {
    /// Max requests per rolling minute window (starts at first use).
    #[serde(default)]
    pub per_minute: Option<u32>,
    /// Max requests per rolling hour window (persisted across restarts).
    #[serde(default)]
    pub per_hour: Option<u32>,
    /// Max concurrent in-flight requests on this key.
    ///
    /// Only meaningful in `RotationMode::Reactive` (proactive mode already
    /// spreads load); combining it with proactive mode is a build error.
    #[serde(default)]
    pub max_concurrency: Option<u32>,
    /// Rare use case: when a lease attempt finds this key saturated, put it
    /// into a health `Cooldown` for this many milliseconds so its reusable
    /// time becomes observable/deterministic. Without this, a saturated key
    /// becomes reusable the instant any in-flight request completes.
    #[serde(default)]
    pub concurrency_cooldown_ms: Option<u64>,
}

impl KeyLimits {
    /// No limits at all.
    pub const fn none() -> Self {
        Self {
            per_minute: None,
            per_hour: None,
            max_concurrency: None,
            concurrency_cooldown_ms: None,
        }
    }

    /// Sets the per-minute request limit.
    #[must_use]
    pub const fn per_minute(mut self, limit: u32) -> Self {
        self.per_minute = Some(limit);
        self
    }

    /// Sets the per-hour request limit.
    #[must_use]
    pub const fn per_hour(mut self, limit: u32) -> Self {
        self.per_hour = Some(limit);
        self
    }

    /// Sets the max concurrent in-flight requests (reactive mode only).
    #[must_use]
    pub const fn max_concurrency(mut self, limit: u32) -> Self {
        self.max_concurrency = Some(limit);
        self
    }

    /// Sets the optional saturation cooldown (see field docs).
    #[must_use]
    pub const fn concurrency_cooldown_ms(mut self, ms: u64) -> Self {
        self.concurrency_cooldown_ms = Some(ms);
        self
    }

    /// Whether any *rate* (window) limit is set — drives quota persistence.
    pub const fn has_rate_limits(&self) -> bool {
        self.per_minute.is_some() || self.per_hour.is_some()
    }

    /// Whether no limit of any kind is set.
    pub const fn is_unlimited(&self) -> bool {
        self.per_minute.is_none() && self.per_hour.is_none() && self.max_concurrency.is_none()
    }
}

/// A single usage window: when it started and how much was used.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowState {
    /// Unix ms at which this window began (first request in the window).
    pub start_ms: i64,
    /// Requests consumed inside this window.
    pub used: u32,
}

impl WindowState {
    /// Whether this window's time has fully passed at `now`.
    pub const fn expired(&self, now_ms: i64, len_ms: i64) -> bool {
        now_ms >= self.start_ms.saturating_add(len_ms)
    }

    /// Unix ms at which this window resets.
    pub const fn resets_at_ms(&self, len_ms: i64) -> i64 {
        self.start_ms.saturating_add(len_ms)
    }
}

/// Is a request currently allowed under this window + limit?
///
/// `None` limit = unlimited. An expired window counts as fresh.
pub(crate) const fn window_usable(
    window: Option<WindowState>,
    limit: Option<u32>,
    now_ms: i64,
    len_ms: i64,
) -> bool {
    match (window, limit) {
        (_, None) | (None, _) => true,
        (Some(w), Some(l)) => w.expired(now_ms, len_ms) || w.used < l,
    }
}

/// Records one request against the window, starting/resetting it as needed.
pub(crate) const fn consume_window(window: &mut Option<WindowState>, now_ms: i64, len_ms: i64) {
    match window {
        Some(w) if !w.expired(now_ms, len_ms) => w.used = w.used.saturating_add(1),
        _ => {
            *window = Some(WindowState {
                start_ms: now_ms,
                used: 1,
            });
        }
    }
}

/// Milliseconds until an exhausted window frees up, if it is the blocker.
pub(crate) fn window_wait_ms(
    window: Option<WindowState>,
    limit: Option<u32>,
    now_ms: i64,
    len_ms: i64,
) -> Option<u64> {
    let (w, l) = (window?, limit?);
    if w.expired(now_ms, len_ms) || w.used < l {
        None
    } else {
        Some(u64::try_from(w.resets_at_ms(len_ms).saturating_sub(now_ms)).unwrap_or(0))
    }
}

// ---------------------------------------------------------------------------
// Serializable live snapshots
// ---------------------------------------------------------------------------

/// Live snapshot of one rate window, suitable for display in a UI.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WindowInfo {
    /// Requests used in the current window (0 if the window expired).
    pub used: u32,
    /// The configured limit, if any.
    pub limit: Option<u32>,
    /// Requests remaining in the current window, if a limit is set.
    pub remaining: Option<u32>,
    /// Unix ms at which the current window resets.
    pub resets_at_ms: i64,
    /// Milliseconds from now until the reset — track this for countdowns.
    pub resets_in_ms: u64,
}

impl WindowInfo {
    pub(crate) fn build(
        window: Option<WindowState>,
        limit: Option<u32>,
        now_ms: i64,
        len_ms: i64,
    ) -> Option<Self> {
        let w = match window {
            // Expired window == fresh: report zero usage from "now".
            Some(w) if w.expired(now_ms, len_ms) => None,
            other => other,
        };
        if w.is_none() && limit.is_none() {
            return None; // nothing to report
        }
        let (used, resets_at_ms) = w.map_or_else(
            || (0, now_ms.saturating_add(len_ms)),
            |w| (w.used, w.resets_at_ms(len_ms)),
        );
        Some(Self {
            used,
            limit,
            remaining: limit.map(|l| l.saturating_sub(used)),
            resets_at_ms,
            resets_in_ms: u64::try_from(resets_at_ms.saturating_sub(now_ms)).unwrap_or(0),
        })
    }
}

/// Full live quota snapshot for one key.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct KeyQuotaInfo {
    /// Minute window state, if the key has a minute limit or recent usage.
    pub minute: Option<WindowInfo>,
    /// Hour window state, if the key has an hour limit or recent usage.
    pub hour: Option<WindowInfo>,
    /// Requests currently in flight on this key.
    pub in_flight: u32,
    /// The configured concurrency cap, if any.
    pub max_concurrency: Option<u32>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_key_is_usable() {
        assert!(window_usable(None, Some(5), 0, MINUTE_MS));
        assert!(window_usable(None, None, 0, MINUTE_MS));
    }

    #[test]
    fn window_counts_up_and_blocks_at_limit() {
        let mut w = None;
        let now = 1_000_000;
        for _ in 0..5 {
            assert!(window_usable(w, Some(5), now, MINUTE_MS));
            consume_window(&mut w, now, MINUTE_MS);
        }
        assert_eq!(w.unwrap().used, 5);
        assert!(!window_usable(w, Some(5), now, MINUTE_MS));
        // 60s later the window has passed: usable again.
        assert!(window_usable(w, Some(5), now + MINUTE_MS, MINUTE_MS));
    }

    #[test]
    fn consume_resets_expired_window() {
        let mut w = Some(WindowState {
            start_ms: 0,
            used: 99,
        });
        consume_window(&mut w, MINUTE_MS + 1, MINUTE_MS);
        let w = w.unwrap();
        assert_eq!(w.used, 1);
        assert_eq!(w.start_ms, MINUTE_MS + 1);
    }

    #[test]
    fn wait_ms_reports_time_to_reset() {
        let w = Some(WindowState {
            start_ms: 0,
            used: 5,
        });
        assert_eq!(window_wait_ms(w, Some(5), 10_000, MINUTE_MS), Some(50_000));
        // Not exhausted -> no wait.
        assert_eq!(window_wait_ms(w, Some(6), 10_000, MINUTE_MS), None);
        // No limit -> no wait.
        assert_eq!(window_wait_ms(w, None, 10_000, MINUTE_MS), None);
    }

    #[test]
    fn window_info_snapshot() {
        let w = Some(WindowState {
            start_ms: 0,
            used: 3,
        });
        let info = WindowInfo::build(w, Some(5), 20_000, MINUTE_MS).unwrap();
        assert_eq!(info.used, 3);
        assert_eq!(info.remaining, Some(2));
        assert_eq!(info.resets_at_ms, 60_000);
        assert_eq!(info.resets_in_ms, 40_000);
        // Expired window reports fresh zero usage.
        let info = WindowInfo::build(w, Some(5), 70_000, MINUTE_MS).unwrap();
        assert_eq!(info.used, 0);
        assert_eq!(info.remaining, Some(5));
        // No usage, no limit -> nothing to report.
        assert!(WindowInfo::build(None, None, 0, MINUTE_MS).is_none());
    }

    #[test]
    fn limits_builder_and_flags() {
        let l = KeyLimits::none().per_minute(5).max_concurrency(2);
        assert!(l.has_rate_limits());
        assert!(!l.is_unlimited());
        assert!(KeyLimits::none().is_unlimited());
        assert!(!KeyLimits::none().max_concurrency(1).has_rate_limits());
    }
}
