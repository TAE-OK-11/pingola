use std::hash::{Hash, Hasher};
use std::net::IpAddr;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;

use dashmap::DashMap;

const MAX_RATE_BUCKETS: usize = 262_144;

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
    observations: AtomicU64,
    max_buckets: usize,
}

impl RateLimiter {
    pub fn new() -> Self {
        Self::with_max_buckets(MAX_RATE_BUCKETS)
    }

    fn with_max_buckets(max_buckets: usize) -> Self {
        Self {
            buckets: DashMap::new(),
            observations: AtomicU64::new(0),
            max_buckets,
        }
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
        let key = ClientKey { zone, ip };
        // Bound memory even during a sustained unique-source flood. Existing
        // clients continue to use their buckets; unseen clients fail closed
        // until the periodic idle cleanup frees capacity.
        if self.buckets.len() >= self.max_buckets && !self.buckets.contains_key(&key) {
            return false;
        }
        let mut bucket = self.buckets.entry(key).or_insert_with(|| Bucket {
            tokens: capacity,
            updated_at: now,
        });
        let elapsed = now.duration_since(bucket.updated_at).as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * requests_per_second).min(capacity);
        bucket.updated_at = now;

        let allowed = bucket.tokens >= 1.0;
        if allowed {
            bucket.tokens -= 1.0;
        }
        drop(bucket);

        // Bound memory during long-running scans. Cleanup is deliberately rare
        // and only removes buckets that have been idle for ten minutes.
        if self.observations.fetch_add(1, Ordering::Relaxed) & 0x3fff == 0x3fff {
            self.buckets
                .retain(|_, bucket| now.duration_since(bucket.updated_at).as_secs() < 600);
        }

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
            .or_insert_with(|| Arc::new(AtomicUsize::new(0)))
            .clone();

        counter
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                (current < limit).then_some(current + 1)
            })
            .ok()?;

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
    fn zones_are_isolated_for_the_same_client() {
        let limiter = ActiveRequestLimiter::new();
        let ip = "192.0.2.3".parse().unwrap();
        let _stream = limiter.acquire("navidrome_stream", ip, 1).unwrap();
        assert!(limiter.acquire("navidrome_stream", ip, 1).is_none());
        assert!(limiter.acquire("vaultwarden", ip, 1).is_some());
        assert!(limiter.acquire("doh", ip, 1).is_some());
    }
}
