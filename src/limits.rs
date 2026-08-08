use std::cell::Cell;
use std::hash::{Hash, Hasher};
use std::net::IpAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use ahash::RandomState;
use dashmap::DashMap;
use dashmap::mapref::entry::Entry;
use parking_lot::Mutex;

const MAX_RATE_BUCKETS: usize = 262_144;
const RATE_BUCKET_IDLE: Duration = Duration::from_secs(600);
const RATE_CLEANUP_INTERVAL: Duration = Duration::from_secs(1);
const RATE_CLEANUP_OBSERVATIONS: u16 = 1 << 14;
const MAX_ACTIVE_COUNTERS: usize = 32_768;

thread_local! {
    // Housekeeping does not require a process-wide sequence number. Keeping the
    // sampling counter per worker removes one contended atomic RMW from every
    // rate-limited H1 request / H2 stream while the capacity-full path still
    // forces cleanup synchronously.
    static RATE_CLEANUP_TICK: Cell<u16> = const { Cell::new(0) };
}

#[derive(Clone, Eq)]
struct ClientKey {
    zone: &'static str,
    ip: IpAddr,
}

impl PartialEq for ClientKey {
    fn eq(&self, other: &Self) -> bool {
        self.zone == other.zone && self.ip == other.ip
    }
}

impl Hash for ClientKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        // Production zones are fixed static strings. Hash a compact tag instead
        // of scanning the same route name on every request. Unknown zones keep
        // their full string hash so tests and future callers remain isolated.
        match production_zone_tag(self.zone) {
            Some(tag) => tag.hash(state),
            None => {
                u8::MAX.hash(state);
                self.zone.hash(state);
            }
        }
        self.ip.hash(state);
    }
}

#[inline]
fn production_zone_tag(zone: &str) -> Option<u8> {
    match zone {
        "global" => Some(1),
        "http3-connection" => Some(2),
        "navidrome_stream" => Some(3),
        "navidrome_cover" => Some(4),
        "navidrome_api" => Some(5),
        "vaultwarden_auth" => Some(6),
        "vaultwarden_hub" => Some(7),
        "vaultwarden" => Some(8),
        "couchdb" => Some(9),
        "doh" => Some(10),
        "adguard_ui" => Some(11),
        _ => None,
    }
}

#[inline]
fn rate_cleanup_sample_due() -> bool {
    RATE_CLEANUP_TICK.with(|tick| {
        let next = tick.get().wrapping_add(1);
        tick.set(next);
        next % RATE_CLEANUP_OBSERVATIONS == 0
    })
}

struct Bucket {
    tokens: f64,
    updated_at: Instant,
}

type RateBucketMap = DashMap<ClientKey, Bucket, RandomState>;
type ActiveCounterMap = DashMap<ClientKey, Arc<AtomicUsize>, RandomState>;

/// Per-client token buckets. The map is sharded so unrelated clients do not
/// contend on a global lock.
pub struct RateLimiter {
    buckets: RateBucketMap,
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
            // AHash is keyed per limiter instance and substantially cheaper than
            // std HashMap's default hasher for the tiny (zone, IP) hot-path key.
            buckets: DashMap::with_hasher(RandomState::new()),
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
        self.bucket_count.fetch_sub(removed, Ordering::Relaxed);
        true
    }

    pub fn allow(
        &self,
        zone: &'static str,
        ip: IpAddr,
        requests_per_second: f64,
        burst: u32,
    ) -> bool {
        debug_assert!(requests_per_second > 0.0);
        let now = Instant::now();
        let capacity = f64::from(burst) + 1.0;
        if rate_cleanup_sample_due() {
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
                        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |count| {
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
    counters: ActiveCounterMap,
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
            counters: DashMap::with_hasher(RandomState::new()),
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
            let keep = counter.load(Ordering::Relaxed) != 0;
            removed += usize::from(!keep);
            keep
        });
        self.counter_count.fetch_sub(removed, Ordering::Relaxed);
        true
    }

    pub fn acquire(
        &self,
        zone: &'static str,
        ip: IpAddr,
        limit: usize,
    ) -> Option<ActiveRequestPermit> {
        let key = ClientKey { zone, ip };

        // Existing clients are the overwhelmingly common path. A read lookup
        // avoids constructing a DashMap entry and cloning ClientKey on every
        // request. Holding the map guard through the atomic increment prevents a
        // capacity cleanup from removing this counter concurrently.
        if let Some(counter) = self.counters.get(&key) {
            counter
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
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
                        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                            (current < limit).then_some(current + 1)
                        })
                        .ok()?;
                    break counter;
                }
                Entry::Vacant(entry) => {
                    if self
                        .counter_count
                        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |count| {
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
        self.counter.fetch_sub(1, Ordering::Relaxed);
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
        assert!(limiter.allow("api", ip, 1.0, 2));
        assert!(limiter.allow("api", ip, 1.0, 2));
        assert!(limiter.allow("api", ip, 1.0, 2));
        assert!(!limiter.allow("api", ip, 1.0, 2));
    }

    #[test]
    fn active_request_permit_releases_capacity() {
        let limiter = ActiveRequestLimiter::new();
        let ip = "192.0.2.2".parse().unwrap();
        let permit = limiter.acquire("host", ip, 1).unwrap();
        assert!(limiter.acquire("host", ip, 1).is_none());
        drop(permit);
        assert!(limiter.acquire("host", ip, 1).is_some());
    }

    #[test]
    fn inactive_active_request_counter_is_reused_without_map_churn() {
        let limiter = ActiveRequestLimiter::new();
        let ip = "192.0.2.4".parse().unwrap();
        drop(limiter.acquire("api", ip, 1).unwrap());
        assert_eq!(limiter.counters.len(), 1);
        drop(limiter.acquire("api", ip, 1).unwrap());
        assert_eq!(limiter.counters.len(), 1);
        assert_eq!(limiter.counter_count.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn inactive_active_request_counter_is_reclaimed_at_capacity() {
        let limiter = ActiveRequestLimiter::with_max_counters(1);
        drop(
            limiter
                .acquire("api", "192.0.2.40".parse().unwrap(), 1)
                .unwrap(),
        );
        assert!(
            limiter
                .acquire("api", "192.0.2.41".parse().unwrap(), 1)
                .is_some()
        );
        assert_eq!(limiter.counter_count.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn rate_bucket_capacity_is_bounded_and_fails_closed_for_new_clients() {
        let limiter = RateLimiter::with_max_buckets(1);
        assert!(limiter.allow("api", "192.0.2.10".parse().unwrap(), 1.0, 0));
        assert!(!limiter.allow("api", "192.0.2.11".parse().unwrap(), 1.0, 0));
    }

    #[test]
    fn idle_rate_bucket_is_reclaimed_when_capacity_is_full() {
        let limiter = RateLimiter::with_max_buckets(1);
        let old_ip = "192.0.2.20".parse().unwrap();
        assert!(limiter.allow("api", old_ip, 1.0, 0));
        limiter
            .buckets
            .get_mut(&ClientKey {
                zone: "api",
                ip: old_ip,
            })
            .unwrap()
            .updated_at = Instant::now() - Duration::from_secs(601);

        assert!(limiter.allow("api", "192.0.2.21".parse().unwrap(), 1.0, 0));
        assert_eq!(limiter.bucket_count.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn production_zone_tags_are_unique() {
        let zones = [
            "global",
            "http3-connection",
            "navidrome_stream",
            "navidrome_cover",
            "navidrome_api",
            "vaultwarden_auth",
            "vaultwarden_hub",
            "vaultwarden",
            "couchdb",
            "doh",
            "adguard_ui",
        ];
        let mut tags = zones
            .iter()
            .map(|zone| production_zone_tag(zone).unwrap())
            .collect::<Vec<_>>();
        tags.sort_unstable();
        tags.dedup();
        assert_eq!(tags.len(), zones.len());
        assert_eq!(production_zone_tag("test-zone"), None);
    }

    #[test]
    fn zones_are_isolated_for_the_same_client() {
        let limiter = ActiveRequestLimiter::new();
        let ip = "192.0.2.3".parse().unwrap();
        let _stream = limiter.acquire("navidrome_stream", ip, 1).unwrap();
        assert!(limiter.acquire("navidrome_stream", ip, 1).is_none());
        assert!(limiter.acquire("vaultwarden", ip, 1).is_some());
        assert!(limiter.acquire("doh", ip, 1).is_some());
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
                    match limiter.acquire("api", ip, 1) {
                        Some(permit) => {
                            if granted.fetch_add(1, Ordering::Relaxed) != 0 {
                                violated.store(true, Ordering::Relaxed);
                            }
                            thread::yield_now();
                            granted.fetch_sub(1, Ordering::Relaxed);
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

        assert!(!violated.load(Ordering::Relaxed));
    }
}
