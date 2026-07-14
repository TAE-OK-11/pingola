use std::hash::{Hash, Hasher};
use std::net::IpAddr;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use dashmap::mapref::entry::Entry;
use dashmap::DashMap;
use parking_lot::Mutex;

const MAX_RATE_BUCKETS: usize = 262_144;
const RATE_BUCKET_IDLE: Duration = Duration::from_secs(600);
const RATE_CLEANUP_INTERVAL: Duration = Duration::from_secs(1);

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
        self.zone.hash(state);
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
    observations: AtomicU64,
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
            observations: AtomicU64::new(0),
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
        if self.observations.fetch_add(1, Ordering::Relaxed) & 0x3fff == 0x3fff {
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

/// Counts active requests per IP. HTTP/2 requests are streams, not TCP
/// connections, so the type intentionally describes what is actually bounded.
pub struct ActiveRequestLimiter {
    counters: Arc<DashMap<ClientKey, Arc<AtomicUsize>>>,
}

impl ActiveRequestLimiter {
    pub fn new() -> Self {
        Self {
            counters: Arc::new(DashMap::new()),
        }
    }

    pub fn acquire(
        &self,
        zone: &'static str,
        ip: IpAddr,
        limit: usize,
    ) -> Option<ActiveRequestPermit> {
        let key = ClientKey { zone, ip };
        let counter = self
            .counters
            .entry(key.clone())
            .or_insert_with(|| Arc::new(AtomicUsize::new(0)));

        counter
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                (current < limit).then_some(current + 1)
            })
            .ok()?;
        let counter = counter.clone();

        Some(ActiveRequestPermit {
            counters: self.counters.clone(),
            key,
            counter,
        })
    }
}

pub struct ActiveRequestPermit {
    counters: Arc<DashMap<ClientKey, Arc<AtomicUsize>>>,
    key: ClientKey,
    counter: Arc<AtomicUsize>,
}

impl Drop for ActiveRequestPermit {
    fn drop(&mut self) {
        if self.counter.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.counters.remove_if(&self.key, |_, current| {
                Arc::ptr_eq(current, &self.counter) && current.load(Ordering::Acquire) == 0
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::sync::Barrier;
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
        assert_eq!(limiter.bucket_count.load(Ordering::Acquire), 1);
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
                    if let Some(permit) = limiter.acquire("api", ip, 1) {
                        if granted.fetch_add(1, Ordering::AcqRel) != 0 {
                            violated.store(true, Ordering::Release);
                        }
                        thread::yield_now();
                        granted.fetch_sub(1, Ordering::AcqRel);
                        drop(permit);
                    } else {
                        thread::yield_now();
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
