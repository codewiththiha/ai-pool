//! Key selection and rotation.
//!
//! Holds the decrypted keys in RAM (`DashMap<String, KeyState>`) and decides
//! *which* key to use for the next request, honoring each key's optional
//! quota limits (per-minute / per-hour windows, per-key concurrency).

pub mod proactive;
pub mod reactive;

use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use dashmap::DashMap;
use secrecy::SecretString;

use crate::config::RotationMode;
use crate::quota::{
    self, HOUR_MS, KeyLimits, KeyQuotaInfo, MINUTE_MS, WindowInfo, WindowState, consume_window,
    window_usable, window_wait_ms,
};
use crate::storage::KeyHealth;

/// Everything needed to (re)load one key into the pool.
pub struct KeySeed {
    /// Stable key id.
    pub id: String,
    /// Decrypted API key.
    pub secret: SecretString,
    /// Censored display form.
    pub censored: String,
    /// Initial health.
    pub health: KeyHealth,
    /// Per-key limits (`None` = pool default).
    pub limits: Option<KeyLimits>,
    /// Persisted minute window, if any.
    pub minute: Option<WindowState>,
    /// Persisted hour window, if any.
    pub hour: Option<WindowState>,
}

impl KeySeed {
    /// A fresh, healthy, unlimited-unless-default key.
    pub fn new(id: impl Into<String>, secret: SecretString, censored: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            secret,
            censored: censored.into(),
            health: KeyHealth::Active,
            limits: None,
            minute: None,
            hour: None,
        }
    }

    /// Sets the initial health.
    #[must_use]
    pub fn health(mut self, health: KeyHealth) -> Self {
        self.health = health;
        self
    }

    /// Sets per-key limits.
    #[must_use]
    pub const fn limits(mut self, limits: Option<KeyLimits>) -> Self {
        self.limits = limits;
        self
    }

    /// Sets persisted windows.
    #[must_use]
    pub const fn windows(
        mut self,
        minute: Option<WindowState>,
        hour: Option<WindowState>,
    ) -> Self {
        self.minute = minute;
        self.hour = hour;
        self
    }
}

/// A decrypted key held in RAM.
pub struct KeyState {
    /// Stable key id.
    pub id: String,
    /// Decrypted API key.
    pub secret: SecretString,
    /// Censored display form.
    pub censored: String,
    /// Live health state.
    pub health: KeyHealth,
    /// Insertion order, used for stable round-robin iteration.
    pub order: usize,
    /// Optional quota limits for this key.
    pub limits: KeyLimits,
    /// Rolling minute window (starts at first use).
    pub minute: Option<WindowState>,
    /// Rolling hour window (persisted across restarts).
    pub hour: Option<WindowState>,
    /// Requests currently in flight on this key.
    pub in_flight: Arc<AtomicU32>,
}

/// Decrements the per-key in-flight counter when the lease is dropped.
struct InFlightGuard(Arc<AtomicU32>);

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// A key leased to the Dispatcher for one attempt.
///
/// Holding the lease counts as one in-flight request against the key's
/// concurrency quota; dropping it releases the slot. Keep it alive for the
/// full duration of the HTTP call (including streaming).
pub struct LeasedKey {
    /// Id of the leased key.
    pub id: String,
    /// Decrypted API key for this attempt.
    pub secret: SecretString,
    _guard: InFlightGuard,
}

/// The live pool: decrypted keys + rotation & quota state.
pub struct KeyPool {
    keys: DashMap<String, KeyState>,
    mode: RotationMode,
    /// Limits applied to keys that have no persisted per-key limits.
    default_limits: KeyLimits,
    /// Proactive: atomic round-robin cursor.
    cursor: AtomicUsize,
    /// Reactive: id of the current sticky "primary".
    primary: RwLock<Option<String>>,
    /// Monotonic insertion counter.
    next_order: AtomicUsize,
    #[cfg(feature = "sqlite")]
    sqlite_pool: RwLock<Option<sqlx::SqlitePool>>,
}

impl KeyPool {
    /// Creates an empty pool for the given rotation mode and default limits.
    pub fn new(mode: RotationMode, default_limits: KeyLimits) -> Self {
        Self {
            keys: DashMap::new(),
            mode,
            default_limits,
            cursor: AtomicUsize::new(0),
            primary: RwLock::new(None),
            next_order: AtomicUsize::new(0),
            #[cfg(feature = "sqlite")]
            sqlite_pool: RwLock::new(None),
        }
    }

    /// The rotation mode this pool was built with.
    pub const fn mode(&self) -> RotationMode {
        self.mode
    }

    /// The default limits applied to keys without per-key limits.
    pub const fn default_limits(&self) -> KeyLimits {
        self.default_limits
    }

    #[cfg(feature = "sqlite")]
    pub(crate) fn register_sqlite_pool(&self, pool: sqlx::SqlitePool) {
        *self.sqlite_pool.write().expect("lock poisoned") = Some(pool);
    }

    #[cfg(feature = "sqlite")]
    pub(crate) fn raw_sqlite_pool(&self) -> Option<sqlx::SqlitePool> {
        self.sqlite_pool.read().expect("lock poisoned").clone()
    }

    /// Insert or replace a key in the live pool.
    ///
    /// `limits: None` applies the pool default; persisted windows (from the
    /// vault) carry hour-quota usage across restarts.
    pub fn upsert(&self, seed: KeySeed) {
        let order = self.keys.get(&seed.id).map_or_else(
            || self.next_order.fetch_add(1, Ordering::Relaxed),
            |existing| existing.order,
        );
        self.keys.insert(
            seed.id.clone(),
            KeyState {
                id: seed.id,
                secret: seed.secret,
                censored: seed.censored,
                health: seed.health,
                order,
                limits: seed.limits.unwrap_or(self.default_limits),
                minute: seed.minute,
                hour: seed.hour,
                in_flight: Arc::new(AtomicU32::new(0)),
            },
        );
    }

    /// Removes a key from the live pool.
    ///
    /// # Panics
    /// Panics only if the internal primary lock is poisoned.
    pub fn remove(&self, id: &str) {
        self.keys.remove(id);
        let mut primary = self.primary.write().expect("lock poisoned");
        if primary.as_deref() == Some(id) {
            *primary = None;
        }
    }

    /// Number of keys currently loaded (any health).
    pub fn len(&self) -> usize {
        self.keys.len()
    }

    /// Whether the pool holds no keys at all.
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// Live health of a key, if it is loaded.
    pub fn health_of(&self, id: &str) -> Option<KeyHealth> {
        self.keys.get(id).map(|k| k.health.clone())
    }

    /// Update a key's live health. Returns `Some(())` when the key exists.
    pub fn set_health(&self, id: &str, health: KeyHealth) -> Option<()> {
        let mut entry = self.keys.get_mut(id)?;
        entry.health = health;
        drop(entry);
        Some(())
    }

    /// Replaces a key's limits. Returns `Some(())` when the key exists.
    pub fn set_limits(&self, id: &str, limits: KeyLimits) -> Option<()> {
        let mut entry = self.keys.get_mut(id)?;
        entry.limits = limits;
        drop(entry);
        Some(())
    }

    /// A key's effective limits, if it is loaded.
    pub fn limits_of(&self, id: &str) -> Option<KeyLimits> {
        self.keys.get(id).map(|k| k.limits)
    }

    /// Whether any loaded key carries a concurrency cap (used to reject
    /// proactive + concurrency configurations).
    pub fn any_concurrency_limit(&self) -> bool {
        self.default_limits.max_concurrency.is_some()
            || self.keys.iter().any(|k| k.limits.max_concurrency.is_some())
    }

    /// Live quota snapshot for one key (windows, in-flight, caps).
    pub fn quota_of(&self, id: &str) -> Option<KeyQuotaInfo> {
        let k = self.keys.get(id)?;
        let now = quota::now_ms();
        Some(KeyQuotaInfo {
            minute: WindowInfo::build(k.minute, k.limits.per_minute, now, MINUTE_MS),
            hour: WindowInfo::build(k.hour, k.limits.per_hour, now, HOUR_MS),
            in_flight: k.in_flight.load(Ordering::Acquire),
            max_concurrency: k.limits.max_concurrency,
        })
    }

    /// Raw window state + limits for persistence after a lease.
    pub fn windows_of(
        &self,
        id: &str,
    ) -> Option<(Option<WindowState>, Option<WindowState>, KeyLimits)> {
        self.keys.get(id).map(|k| (k.minute, k.hour, k.limits))
    }

    /// Ids of keys that are health-usable AND inside their rate windows,
    /// in stable insertion order.
    fn rate_usable_ids(&self, now_instant: Instant, now_ms: i64) -> Vec<(usize, String)> {
        let mut ids: Vec<(usize, String)> = self
            .keys
            .iter()
            .filter(|k| {
                k.health.is_usable(now_instant)
                    && window_usable(k.minute, k.limits.per_minute, now_ms, MINUTE_MS)
                    && window_usable(k.hour, k.limits.per_hour, now_ms, HOUR_MS)
            })
            .map(|k| (k.order, k.id.clone()))
            .collect();
        ids.sort_unstable();
        ids
    }

    fn is_saturated(&self, id: &str) -> bool {
        self.keys.get(id).is_some_and(|k| {
            k.limits
                .max_concurrency
                .is_some_and(|maxc| k.in_flight.load(Ordering::Acquire) >= maxc)
        })
    }

    /// Marks a concurrency-saturated key as cooling for its configured
    /// `concurrency_cooldown_ms`, making its reusable time observable.
    fn apply_saturation_cooldown(&self, id: &str) {
        let Some(mut k) = self.keys.get_mut(id) else {
            return;
        };
        if let Some(ms) = k.limits.concurrency_cooldown_ms {
            if matches!(k.health, KeyHealth::Active) {
                k.health = KeyHealth::Cooldown {
                    retry_after: Instant::now() + Duration::from_millis(ms),
                };
            }
        }
    }

    /// Picks the next key according to the configured rotation mode and every
    /// quota gate, then leases it: rate windows are consumed and the
    /// in-flight counter incremented (released when the lease drops).
    ///
    /// Returns `None` when no key is currently usable — inspect
    /// [`Self::earliest_recovery_ms`] to distinguish "wait" from "dead".
    pub fn next_key(&self) -> Option<LeasedKey> {
        // Small retry loop: selection and lease are separate critical
        // sections, so a concurrent lease can race a concurrency slot away.
        for _ in 0..4 {
            let now_instant = Instant::now();
            let now = quota::now_ms();
            let usable = self.rate_usable_ids(now_instant, now);
            if usable.is_empty() {
                return None;
            }

            let chosen = match self.mode {
                RotationMode::Proactive => proactive::pick(&self.cursor, &usable),
                RotationMode::Reactive => {
                    // Concurrency-aware sticky failover: skip saturated keys
                    // instead of waiting for a 429 to force rotation.
                    let picked =
                        reactive::pick(&self.primary, &usable, |id| self.is_saturated(id));
                    if let Some(id) = picked {
                        id
                    } else {
                        // Every rate-usable key is concurrency-saturated.
                        for (_, id) in &usable {
                            self.apply_saturation_cooldown(id);
                        }
                        return None;
                    }
                }
            };

            if let Some(lease) = self.try_lease(&chosen, now) {
                return Some(lease);
            }
            // Raced: the chosen key filled up between selection and lease.
        }
        None
    }

    /// Attempts to lease `id` right now: re-validates quota gates under the
    /// entry lock, consumes windows, and increments in-flight.
    fn try_lease(&self, id: &str, now: i64) -> Option<LeasedKey> {
        let mut k = self.keys.get_mut(id)?;

        // Re-check gates under the shard lock (selection ran lock-free).
        if !window_usable(k.minute, k.limits.per_minute, now, MINUTE_MS)
            || !window_usable(k.hour, k.limits.per_hour, now, HOUR_MS)
        {
            return None;
        }
        if let Some(maxc) = k.limits.max_concurrency {
            if k.in_flight.load(Ordering::Acquire) >= maxc {
                return None;
            }
        }

        if k.limits.per_minute.is_some() {
            consume_window(&mut k.minute, now, MINUTE_MS);
        }
        if k.limits.per_hour.is_some() {
            consume_window(&mut k.hour, now, HOUR_MS);
        }
        k.in_flight.fetch_add(1, Ordering::AcqRel);

        Some(LeasedKey {
            id: k.id.clone(),
            secret: k.secret.clone(),
            _guard: InFlightGuard(Arc::clone(&k.in_flight)),
        })
    }

    /// Milliseconds until the soonest key becomes usable again, if that time
    /// is knowable (health cooldowns and exhausted rate windows; concurrency
    /// saturation has no deterministic end unless a cooldown was configured).
    ///
    /// `None` means either a key is usable right now, or every blocked key is
    /// blocked indefinitely (banned / saturated without cooldown).
    pub fn earliest_recovery_ms(&self) -> Option<u64> {
        let now_instant = Instant::now();
        let now = quota::now_ms();
        self.keys
            .iter()
            .filter_map(|k| {
                let mut waits: Vec<u64> = Vec::new();
                match &k.health {
                    KeyHealth::Banned { .. } => return None,
                    KeyHealth::Cooldown { retry_after } => {
                        waits.push(
                            u64::try_from(
                                retry_after
                                    .saturating_duration_since(now_instant)
                                    .as_millis(),
                            )
                            .unwrap_or(u64::MAX),
                        );
                    }
                    KeyHealth::Active => {}
                }
                if let Some(w) = window_wait_ms(k.minute, k.limits.per_minute, now, MINUTE_MS) {
                    waits.push(w);
                }
                if let Some(w) = window_wait_ms(k.hour, k.limits.per_hour, now, HOUR_MS) {
                    waits.push(w);
                }
                // The key is usable once ALL its blockers clear.
                waits.into_iter().max()
            })
            .min()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool(mode: RotationMode, n: usize) -> KeyPool {
        pool_with_limits(mode, n, KeyLimits::none())
    }

    fn pool_with_limits(mode: RotationMode, n: usize, limits: KeyLimits) -> KeyPool {
        let p = KeyPool::new(mode, limits);
        for i in 0..n {
            p.upsert(KeySeed::new(
                format!("k{i}"),
                SecretString::from(format!("sk-{i}")),
                "sk-...",
            ));
        }
        p
    }

    #[test]
    fn proactive_round_robins() {
        let p = pool(RotationMode::Proactive, 3);
        let picks: Vec<String> = (0..6).map(|_| p.next_key().unwrap().id).collect();
        assert_eq!(picks, ["k0", "k1", "k2", "k0", "k1", "k2"]);
    }

    #[test]
    fn reactive_sticks_until_failure() {
        let p = pool(RotationMode::Reactive, 3);
        assert_eq!(p.next_key().unwrap().id, "k0");
        assert_eq!(p.next_key().unwrap().id, "k0");
        p.set_health(
            "k0",
            KeyHealth::Banned {
                reason: "401".into(),
            },
        );
        assert_eq!(p.next_key().unwrap().id, "k1");
        assert_eq!(p.next_key().unwrap().id, "k1");
    }

    #[test]
    fn exhausted_pool_returns_none() {
        let p = pool(RotationMode::Proactive, 2);
        p.set_health("k0", KeyHealth::Banned { reason: "x".into() });
        p.set_health(
            "k1",
            KeyHealth::Cooldown {
                retry_after: Instant::now() + Duration::from_secs(60),
            },
        );
        assert!(p.next_key().is_none());
        let wait = p.earliest_recovery_ms().unwrap();
        assert!(wait > 55_000 && wait <= 60_000, "wait was {wait}");
    }

    #[test]
    fn expired_cooldown_auto_recovers() {
        let p = pool(RotationMode::Proactive, 1);
        p.set_health(
            "k0",
            KeyHealth::Cooldown {
                retry_after: Instant::now()
                    .checked_sub(Duration::from_secs(1))
                    .unwrap(),
            },
        );
        assert!(p.next_key().is_some());
    }

    #[test]
    fn minute_limit_blocks_then_reports_wait() {
        let p = pool_with_limits(
            RotationMode::Reactive,
            1,
            KeyLimits::none().per_minute(3),
        );
        for _ in 0..3 {
            assert!(p.next_key().is_some());
        }
        assert!(p.next_key().is_none(), "4th lease must be quota-blocked");
        let wait = p.earliest_recovery_ms().unwrap();
        assert!(wait > 0 && wait <= 60_000, "wait was {wait}");

        let q = p.quota_of("k0").unwrap();
        let minute = q.minute.unwrap();
        assert_eq!(minute.used, 3);
        assert_eq!(minute.remaining, Some(0));
        assert!(minute.resets_in_ms > 0);
    }

    #[test]
    fn hour_limit_tracks_independently() {
        let p = pool_with_limits(
            RotationMode::Reactive,
            1,
            KeyLimits::none().per_minute(10).per_hour(2),
        );
        assert!(p.next_key().is_some());
        assert!(p.next_key().is_some());
        assert!(p.next_key().is_none(), "hour quota must block third lease");
        let q = p.quota_of("k0").unwrap();
        assert_eq!(q.hour.unwrap().used, 2);
        assert_eq!(q.minute.unwrap().used, 2);
    }

    #[test]
    fn quota_blocked_key_rotates_to_next() {
        let p = pool_with_limits(
            RotationMode::Reactive,
            2,
            KeyLimits::none().per_minute(1),
        );
        assert_eq!(p.next_key().unwrap().id, "k0");
        // k0 exhausted its minute quota: reactive must fail over to k1.
        assert_eq!(p.next_key().unwrap().id, "k1");
        assert!(p.next_key().is_none());
    }

    #[test]
    fn concurrency_splits_reactive_traffic() {
        let p = pool_with_limits(
            RotationMode::Reactive,
            2,
            KeyLimits::none().max_concurrency(2),
        );
        let lease1 = p.next_key().unwrap();
        let lease2 = p.next_key().unwrap();
        assert_eq!(lease1.id, "k0");
        assert_eq!(lease2.id, "k0");
        // k0 saturated (2 in flight): overflow spills to k1 immediately.
        let lease3 = p.next_key().unwrap();
        assert_eq!(lease3.id, "k1");
        let lease4 = p.next_key().unwrap();
        assert_eq!(lease4.id, "k1");
        // Everything saturated now.
        assert!(p.next_key().is_none());
        // Dropping a lease frees a slot on the (sticky) primary.
        drop(lease1);
        assert_eq!(p.next_key().unwrap().id, "k0");
    }

    #[test]
    fn lease_drop_releases_in_flight() {
        let p = pool_with_limits(
            RotationMode::Reactive,
            1,
            KeyLimits::none().max_concurrency(1),
        );
        let lease = p.next_key().unwrap();
        assert_eq!(p.quota_of("k0").unwrap().in_flight, 1);
        assert!(p.next_key().is_none());
        drop(lease);
        assert_eq!(p.quota_of("k0").unwrap().in_flight, 0);
        assert!(p.next_key().is_some());
    }

    #[test]
    fn saturation_cooldown_marks_key() {
        let p = pool_with_limits(
            RotationMode::Reactive,
            1,
            KeyLimits::none()
                .max_concurrency(1)
                .concurrency_cooldown_ms(30_000),
        );
        let _lease = p.next_key().unwrap();
        assert!(p.next_key().is_none());
        // The saturated key was put into an observable cooldown.
        assert!(matches!(
            p.health_of("k0").unwrap(),
            KeyHealth::Cooldown { .. }
        ));
        assert!(p.earliest_recovery_ms().is_some());
    }

    #[test]
    fn unlimited_keys_ignore_quota_machinery() {
        let p = pool(RotationMode::Reactive, 1);
        for _ in 0..100 {
            assert!(p.next_key().is_some());
        }
        let q = p.quota_of("k0").unwrap();
        assert!(q.minute.is_none());
        assert!(q.hour.is_none());
        assert_eq!(q.in_flight, 0, "all leases dropped");
    }

    #[test]
    fn per_key_limits_override_default() {
        let p = pool_with_limits(
            RotationMode::Reactive,
            2,
            KeyLimits::none().per_minute(1),
        );
        p.set_limits("k1", KeyLimits::none()); // k1 unlimited
        assert_eq!(p.next_key().unwrap().id, "k0");
        for _ in 0..5 {
            assert_eq!(p.next_key().unwrap().id, "k1");
        }
    }
}
