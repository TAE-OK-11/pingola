use std::hash::{Hash, Hasher};
use std::net::IpAddr;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;

use dashmap::DashMap;
use parking_lot::Mutex;

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
    buckets: DashMap<ClientKey, Mutex<Bucket>>,
    observations: AtomicU64,
}

impl RateLimiter {
    pub fn new() -> Self {
        Self {
            buckets: DashMap::new(),
            observations: AtomicU64::new(0),
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
        let entry = self.buckets.entry(key).or_insert_with(|| {
            Mutex::new(Bucket {
                tokens: capacity,
                updated_at: now,
            })
        });
        let mut bucket = entry.lock();
        let elapsed = now.duration_since(bucket.updated_at).as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * requests_per_second).min(capacity);
        bucket.updated_at = now;

        let allowed = bucket.tokens >= 1.0;
        if allowed {
            bucket.tokens -= 1.0;
        }
        drop(bucket);
        drop(entry);

        // Bound memory during long-running scans. Cleanup is deliberately rare
        // and only removes buckets that have been idle for ten minutes.
        if self.observations.fetch_add(1, Ordering::Relaxed) & 0x3fff == 0x3fff {
            self.buckets
                .retain(|_, bucket| now.duration_since(bucket.lock().updated_at).as_secs() < 600);
        }

        allowed
    }
}

/// Counts active requests per IP. For HTTP/1.1 this tracks connections closely;
/// for HTTP/2 it intentionally counts streams, which is the safer bound.
pub struct ConnectionLimiter {
    counters: Arc<DashMap<ClientKey, Arc<AtomicUsize>>>,
}

impl ConnectionLimiter {
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
    ) -> Option<ConnectionPermit> {
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

        Some(ConnectionPermit {
            counters: self.counters.clone(),
            key,
            counter,
        })
    }
}

pub struct ConnectionPermit {
    counters: Arc<DashMap<ClientKey, Arc<AtomicUsize>>>,
    key: ClientKey,
    counter: Arc<AtomicUsize>,
}

impl Drop for ConnectionPermit {
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
    fn connection_permit_releases_capacity() {
        let limiter = ConnectionLimiter::new();
        let ip = "192.0.2.2".parse().unwrap();
        let permit = limiter.acquire("host", ip, 1).unwrap();
        assert!(limiter.acquire("host", ip, 1).is_none());
        drop(permit);
        assert!(limiter.acquire("host", ip, 1).is_some());
    }
}
