use std::cell::Cell;
use std::hash::{Hash, Hasher};
use std::net::IpAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use dashmap::DashMap;
use dashmap::mapref::entry::Entry;
use parking_lot::Mutex;

const MAX_RATE_BUCKETS: usize = 32_768;
const RATE_BUCKET_IDLE: Duration = Duration::from_secs(120);
const RATE_CLEANUP_INTERVAL: Duration = Duration::from_secs(1);
const MAX_ACTIVE_COUNTERS: usize = 4_096;

thread_local! {
    // Worker-local sampling avoids a shared atomic increment on every
    // rate-limited request. Capacity-full vacant inserts still force a
    // synchronous idle scan so the map cannot grow without bound.
    static RATE_CLEANUP_TICK: Cell<u32> = const { Cell::new(0) };
}

/// Compact admission zone. Hashed as a single byte so route-name strings
/// stay off the request hot path.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[repr(u8)]
pub enum LimitZone {
    Static = 1,
    Http3Connection = 2,
    NavidromeStream = 3,
    NavidromeCover = 4,
    NavidromeApi = 5,
    NavidromeGrpc = 12,
    VaultwardenAuth = 6,
    VaultwardenHub = 7,
    Vaultwarden = 8,
    Couchdb = 9,
    Doh = 10,
    AdguardUi = 11,
}

#[derive(Clone, Eq)]
struct ClientKey {
    zone: LimitZone,
    ip: IpAddr,
}

impl PartialEq for ClientKey {
    fn eq(&self, other: &Self) -> bool {
        self.zone == other.zone && self.ip == other.ip
    }
}

impl Hash for ClientKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        (self.zone as u8).hash(state);
        self.ip.hash(state);
    }
}

struct Bucket {
    tokens: f64,
    updated_at: Instant,
}

/// Per-client token buckets. The map is sharded so unrelated clients do not
/// contend on a global lock.
pub struct RateLimiter {
    buckets: DashMap<ClientKey, Bucket>,
    bucket_count: AtomicUsize,
    max_buckets: usize,
    last_cleanup: Mutex<Instant>,
}

impl RateLimiter {
    pub fn new() -> Self {
        Self::with_max_buckets(MAX_RATE_BUCKETS)
    }

    fn with_max_buckets(max_buckets: usize) -> Self {
        let now = Instant::now();
        Self {
            buckets: DashMap::new(),
            bucket_count: AtomicUsize::new(0),
            max_buckets,
            last_cleanup: Mutex::new(now.checked_sub(RATE_CLEANUP_INTERVAL).unwrap_or(now)),
        }
    }

    fn cleanup_idle(&self, now: Instant) -> bool {
        let Some(mut last_cleanup) = self.last_cleanup.try_lock() else {
            return false;
        };
        if now.saturating_duration_since(*last_cleanup) < RATE_CLEANUP_INTERVAL {
            return false;
        }
        *last_cleanup = now;

        let mut removed = 0;
        self.buckets.retain(|_, bucket| {
            let keep = now.saturating_duration_since(bucket.updated_at) < RATE_BUCKET_IDLE;
            removed += usize::from(!keep);
            keep
        });
        self.bucket_count.fetch_sub(removed, Ordering::AcqRel);
        true
    }

    pub fn allow(&self, zone: LimitZone, ip: IpAddr, requests_per_second: f64, burst: u32) -> bool {
        debug_assert!(requests_per_second > 0.0);
        let now = Instant::now();
        let capacity = f64::from(burst) + 1.0;
        let tick = RATE_CLEANUP_TICK.with(|counter| {
            let next = counter.get().wrapping_add(1);
            counter.set(next);
            next
        });
        if tick & 0x3fff == 0x3fff {
            self.cleanup_idle(now);
        }

        // Use a single sharded-map lookup. New clients atomically reserve one
        // bounded slot; existing clients do not pay for len()+contains_key().
        // When the map is full, release the vacant-entry shard guard before a
        // rate-limited idle scan so DashMap cannot deadlock on re-entry.
        let mut retried_after_cleanup = false;
        let mut bucket = loop {
            match self.buckets.entry(ClientKey { zone, ip }) {
                Entry::Occupied(entry) => break entry.into_ref(),
                Entry::Vacant(entry) => {
                    if self
                        .bucket_count
                        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                            (count < self.max_buckets).then_some(count + 1)
                        })
                        .is_ok()
                    {
                        break entry.insert(Bucket {
                            tokens: capacity,
                            updated_at: now,
                        });
                    }
                    drop(entry);
                    if retried_after_cleanup || !self.cleanup_idle(now) {
                        return false;
                    }
                    retried_after_cleanup = true;
                }
            }
        };
        let elapsed = now.duration_since(bucket.updated_at).as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * requests_per_second).min(capacity);
        bucket.updated_at = now;

        let allowed = bucket.tokens >= 1.0;
        if allowed {
            bucket.tokens -= 1.0;
        }
        drop(bucket);

        allowed
    }
}

/// Counts active resources per IP. It is used for HTTP requests/streams and
/// for QUIC connections, with the `zone` separating independent limits.
pub struct ActiveRequestLimiter {
    counters: DashMap<ClientKey, Arc<AtomicUsize>>,
    counter_count: AtomicUsize,
    max_counters: usize,
    cleanup: Mutex<()>,
}

impl ActiveRequestLimiter {
    pub fn new() -> Self {
        Self::with_max_counters(MAX_ACTIVE_COUNTERS)
    }

    fn with_max_counters(max_counters: usize) -> Self {
        Self {
            counters: DashMap::new(),
            counter_count: AtomicUsize::new(0),
            max_counters,
            cleanup: Mutex::new(()),
        }
    }

    fn cleanup_inactive(&self) -> bool {
        let Some(_cleanup) = self.cleanup.try_lock() else {
            return false;
        };
        let mut removed = 0;
        self.counters.retain(|_, counter| {
            let keep = counter.load(Ordering::Acquire) != 0;
            removed += usize::from(!keep);
            keep
        });
        self.counter_count.fetch_sub(removed, Ordering::AcqRel);
        true
    }

    pub fn acquire(
        &self,
        zone: LimitZone,
        ip: IpAddr,
        limit: usize,
    ) -> Option<ActiveRequestPermit> {
        let key = ClientKey { zone, ip };

        // Existing clients are the overwhelmingly common path. A read lookup
        // avoids constructing a DashMap entry and cloning ClientKey on every
        // request. Holding the map guard through the increment prevents a
        // capacity cleanup from removing this counter concurrently.
        if let Some(counter) = self.counters.get(&key) {
            counter
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                    (current < limit).then_some(current + 1)
                })
                .ok()?;
            return Some(ActiveRequestPermit {
                counter: counter.clone(),
            });
        }

        let mut retried_after_cleanup = false;
        let counter = loop {
            let entry = self.counters.entry(key.clone());
            match entry {
                Entry::Occupied(entry) => {
                    let counter = entry.get().clone();
                    counter
                        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                            (current < limit).then_some(current + 1)
                        })
                        .ok()?;
                    break counter;
                }
                Entry::Vacant(entry) => {
                    if self
                        .counter_count
                        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                            (count < self.max_counters).then_some(count + 1)
                        })
                        .is_ok()
                    {
                        let counter = Arc::new(AtomicUsize::new(1));
                        entry.insert(counter.clone());
                        break counter;
                    }
                    drop(entry);
                    if retried_after_cleanup || !self.cleanup_inactive() {
                        return None;
                    }
                    retried_after_cleanup = true;
                }
            }
        };

        Some(ActiveRequestPermit { counter })
    }
}

pub struct ActiveRequestPermit {
    counter: Arc<AtomicUsize>,
}

impl Drop for ActiveRequestPermit {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Process-wide concurrent request cap. Unlike [`ActiveRequestLimiter`], this is
/// not keyed by client IP so HTTP/2 multiplexing on one connection does not
/// consume one slot per stream from the same address.
pub struct GlobalConcurrentLimiter {
    active: Arc<AtomicUsize>,
}

impl GlobalConcurrentLimiter {
    pub fn new() -> Self {
        Self {
            active: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub fn acquire(&self, limit: usize) -> Option<GlobalConcurrentPermit> {
        self.active
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                (current < limit).then_some(current + 1)
            })
            .ok()
            .map(|_| GlobalConcurrentPermit {
                counter: self.active.clone(),
            })
    }
}

pub struct GlobalConcurrentPermit {
    counter: Arc<AtomicUsize>,
}

impl Drop for GlobalConcurrentPermit {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::AcqRel);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Barrier;
    use std::sync::atomic::AtomicBool;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn token_bucket_honors_burst() {
        let limiter = RateLimiter::new();
        let ip = "192.0.2.1".parse().unwrap();
        assert!(limiter.allow(LimitZone::NavidromeApi, ip, 1.0, 2));
        assert!(limiter.allow(LimitZone::NavidromeApi, ip, 1.0, 2));
        assert!(limiter.allow(LimitZone::NavidromeApi, ip, 1.0, 2));
        assert!(!limiter.allow(LimitZone::NavidromeApi, ip, 1.0, 2));
    }

    #[test]
    fn active_request_permit_releases_capacity() {
        let limiter = ActiveRequestLimiter::new();
        let ip = "192.0.2.2".parse().unwrap();
        let permit = limiter.acquire(LimitZone::Vaultwarden, ip, 1).unwrap();
        assert!(limiter.acquire(LimitZone::Vaultwarden, ip, 1).is_none());
        drop(permit);
        assert!(limiter.acquire(LimitZone::Vaultwarden, ip, 1).is_some());
    }

    #[test]
    fn inactive_active_request_counter_is_reused_without_map_churn() {
        let limiter = ActiveRequestLimiter::new();
        let ip = "192.0.2.4".parse().unwrap();
        drop(limiter.acquire(LimitZone::NavidromeApi, ip, 1).unwrap());
        assert_eq!(limiter.counters.len(), 1);
        drop(limiter.acquire(LimitZone::NavidromeApi, ip, 1).unwrap());
        assert_eq!(limiter.counters.len(), 1);
        assert_eq!(limiter.counter_count.load(Ordering::Acquire), 1);
    }

    #[test]
    fn inactive_active_request_counter_is_reclaimed_at_capacity() {
        let limiter = ActiveRequestLimiter::with_max_counters(1);
        drop(
            limiter
                .acquire(LimitZone::NavidromeApi, "192.0.2.40".parse().unwrap(), 1)
                .unwrap(),
        );
        assert!(
            limiter
                .acquire(LimitZone::NavidromeApi, "192.0.2.41".parse().unwrap(), 1)
                .is_some()
        );
        assert_eq!(limiter.counter_count.load(Ordering::Acquire), 1);
    }

    #[test]
    fn rate_bucket_capacity_is_bounded_and_fails_closed_for_new_clients() {
        let limiter = RateLimiter::with_max_buckets(1);
        assert!(limiter.allow(
            LimitZone::NavidromeApi,
            "192.0.2.10".parse().unwrap(),
            1.0,
            0
        ));
        assert!(!limiter.allow(
            LimitZone::NavidromeApi,
            "192.0.2.11".parse().unwrap(),
            1.0,
            0
        ));
    }

    #[test]
    fn idle_rate_bucket_is_reclaimed_when_capacity_is_full() {
        let limiter = RateLimiter::with_max_buckets(1);
        let old_ip = "192.0.2.20".parse().unwrap();
        assert!(limiter.allow(LimitZone::NavidromeApi, old_ip, 1.0, 0));
        limiter
            .buckets
            .get_mut(&ClientKey {
                zone: LimitZone::NavidromeApi,
                ip: old_ip,
            })
            .unwrap()
            .updated_at = Instant::now() - Duration::from_secs(601);

        assert!(limiter.allow(
            LimitZone::NavidromeApi,
            "192.0.2.21".parse().unwrap(),
            1.0,
            0
        ));
        assert_eq!(limiter.bucket_count.load(Ordering::Acquire), 1);
    }

    #[test]
    fn zones_are_isolated_for_the_same_client() {
        let limiter = ActiveRequestLimiter::new();
        let ip = "192.0.2.3".parse().unwrap();
        let _stream = limiter.acquire(LimitZone::NavidromeStream, ip, 1).unwrap();
        assert!(limiter.acquire(LimitZone::NavidromeStream, ip, 1).is_none());
        assert!(limiter.acquire(LimitZone::Vaultwarden, ip, 1).is_some());
        assert!(limiter.acquire(LimitZone::Doh, ip, 1).is_some());
    }

    #[test]
    fn global_concurrent_limiter_is_not_per_client() {
        let limiter = GlobalConcurrentLimiter::new();
        let first = limiter.acquire(2).unwrap();
        let second = limiter.acquire(2).unwrap();
        assert!(limiter.acquire(2).is_none());
        drop(first);
        assert!(limiter.acquire(2).is_some());
        drop(second);
    }

    #[test]
    fn concurrent_release_and_acquire_never_split_one_limit_counter() {
        const THREADS: usize = 8;
        const ITERATIONS: usize = 100_000;

        let limiter = Arc::new(ActiveRequestLimiter::new());
        let barrier = Arc::new(Barrier::new(THREADS));
        let granted = Arc::new(AtomicUsize::new(0));
        let violated = Arc::new(AtomicBool::new(false));
        let ip = "192.0.2.30".parse().unwrap();
        let mut workers = Vec::new();

        for _ in 0..THREADS {
            let limiter = limiter.clone();
            let barrier = barrier.clone();
            let granted = granted.clone();
            let violated = violated.clone();
            workers.push(thread::spawn(move || {
                barrier.wait();
                for _ in 0..ITERATIONS {
                    match limiter.acquire(LimitZone::NavidromeApi, ip, 1) {
                        Some(permit) => {
                            if granted.fetch_add(1, Ordering::AcqRel) != 0 {
                                violated.store(true, Ordering::Release);
                            }
                            thread::yield_now();
                            granted.fetch_sub(1, Ordering::AcqRel);
                            drop(permit);
                        }
                        _ => {
                            thread::yield_now();
                        }
                    }
                }
            }));
        }
        for worker in workers {
            worker.join().unwrap();
        }

        assert!(!violated.load(Ordering::Acquire));
    }
}
