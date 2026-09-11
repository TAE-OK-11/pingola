use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use ahash::AHasher;
use bytes::Bytes;
use dashmap::DashMap;
use pingora_lru::Lru;
use std::hash::Hasher;

use crate::cache::coalesce::{CoalesceGuard, CoalescePermit};
use crate::cache::metrics::NamespaceMetrics;

const LRU_SHARDS: usize = 16;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
#[repr(u8)]
pub enum CacheNamespace {
    Dns = 1,
    Navidrome = 2,
}

impl CacheNamespace {
    pub fn name(self) -> &'static str {
        match self {
            Self::Dns => "dns",
            Self::Navidrome => "navidrome",
        }
    }

    fn metrics<'a>(self, cache: &'a PingolaCache) -> &'a NamespaceMetrics {
        match self {
            Self::Dns => &cache.dns_metrics,
            Self::Navidrome => &cache.navidrome_metrics,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CacheKey {
    pub namespace: CacheNamespace,
    pub hash: u64,
}

impl CacheKey {
    pub fn new(namespace: CacheNamespace, digest: u64) -> Self {
        Self {
            namespace,
            hash: digest,
        }
    }

    pub fn hash_bytes(namespace: CacheNamespace, bytes: &[u8]) -> u64 {
        let mut hasher = AHasher::default();
        hasher.write_u8(namespace as u8);
        hasher.write(bytes);
        hasher.finish()
    }
}

#[derive(Clone)]
pub struct CachedValue {
    pub body: Bytes,
    pub stored_at: Instant,
    pub fresh_until: Instant,
}

impl CachedValue {
    pub fn weight(&self) -> usize {
        self.body.len().max(1)
    }

    pub fn is_fresh(&self, now: Instant) -> bool {
        now < self.fresh_until
    }
}

pub enum CacheLookup {
    Hit(CachedValue),
    Miss,
    Expired,
}

struct StoredEntry {
    namespace: CacheNamespace,
    value: CachedValue,
}

pub struct PingolaCache {
    enabled: bool,
    memory_limit: usize,
    data: DashMap<u64, StoredEntry>,
    order: Lru<u64, LRU_SHARDS>,
    bytes_used: AtomicUsize,
    dns_metrics: NamespaceMetrics,
    navidrome_metrics: NamespaceMetrics,
    coalesce: CoalesceGuard,
}

impl PingolaCache {
    pub fn new(enabled: bool, memory_bytes: usize) -> Arc<Self> {
        let memory_limit = memory_bytes.max(LRU_SHARDS * 64);
        let per_shard = (memory_limit / LRU_SHARDS / 64).max(64);
        Arc::new(Self {
            enabled,
            memory_limit,
            data: DashMap::new(),
            order: Lru::with_capacity(memory_limit, per_shard),
            bytes_used: AtomicUsize::new(0),
            dns_metrics: NamespaceMetrics::default(),
            navidrome_metrics: NamespaceMetrics::default(),
            coalesce: CoalesceGuard::new(),
        })
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn memory_limit(&self) -> usize {
        self.memory_limit
    }

    pub fn lookup(&self, key: &CacheKey, now: Instant) -> CacheLookup {
        if !self.enabled {
            return CacheLookup::Miss;
        }
        let started = Instant::now();
        let metrics = key.namespace.metrics(self);
        let Some(entry) = self.data.get(&key.hash) else {
            metrics.record_miss(started.elapsed().as_nanos() as u64);
            return CacheLookup::Miss;
        };
        if entry.value.is_fresh(now) {
            metrics.record_hit(started.elapsed().as_nanos() as u64);
            let _ = self.order.promote(key.hash);
            CacheLookup::Hit(entry.value.clone())
        } else {
            metrics.record_expiration();
            metrics.record_miss(started.elapsed().as_nanos() as u64);
            drop(entry);
            self.remove(key);
            CacheLookup::Expired
        }
    }

    pub fn insert(&self, key: CacheKey, value: CachedValue) -> bool {
        if !self.enabled {
            return false;
        }
        let weight = value.weight();
        if weight > self.memory_limit {
            key.namespace.metrics(self).record_rejection();
            return false;
        }
        if let Some(old) = self.data.insert(
            key.hash,
            StoredEntry {
                namespace: key.namespace,
                value,
            },
        ) {
            self.bytes_used
                .fetch_sub(old.value.weight(), Ordering::Relaxed);
        }
        self.bytes_used.fetch_add(weight, Ordering::Relaxed);
        self.order.admit(key.hash, key.hash, weight);
        let evicted = self.order.evict_to_limit();
        if !evicted.is_empty() {
            key.namespace
                .metrics(self)
                .record_eviction(evicted.len() as u64);
            for (evicted_key, _) in evicted {
                if let Some((_, entry)) = self.data.remove(&evicted_key) {
                    self.bytes_used
                        .fetch_sub(entry.value.weight(), Ordering::Relaxed);
                }
            }
        }
        key.namespace.metrics(self).record_insert();
        true
    }

    pub fn reject(&self, namespace: CacheNamespace) {
        if self.enabled {
            namespace.metrics(self).record_rejection();
        }
    }

    pub fn remove(&self, key: &CacheKey) {
        if let Some((_, entry)) = self.data.remove(&key.hash) {
            self.bytes_used
                .fetch_sub(entry.value.weight(), Ordering::Relaxed);
            let _ = self.order.remove(key.hash);
        }
    }

    pub fn entries(&self) -> u64 {
        self.data.len() as u64
    }

    pub fn bytes(&self) -> u64 {
        self.bytes_used.load(Ordering::Relaxed) as u64
    }

    pub fn dns_metrics(&self) -> crate::cache::metrics::CacheMetricsSnapshot {
        self.dns_metrics.snapshot(self.entries(), self.bytes())
    }

    pub fn navidrome_metrics(&self) -> crate::cache::metrics::CacheMetricsSnapshot {
        self.navidrome_metrics
            .snapshot(self.entries(), self.bytes())
    }

    pub fn begin_fill(&self, key: CacheKey) -> CoalescePermit {
        self.coalesce.begin(key.hash, key.namespace.metrics(self))
    }

    pub fn finish_fill(&self, permit: CoalescePermit, inserted: bool) {
        self.coalesce.finish(permit, inserted);
    }
}

pub type CacheHandle = Arc<PingolaCache>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_lookup_and_expire() {
        let cache = PingolaCache::new(true, 4096);
        let key = CacheKey::new(CacheNamespace::Dns, 42);
        let now = Instant::now();
        assert!(matches!(cache.lookup(&key, now), CacheLookup::Miss));

        cache.insert(
            key,
            CachedValue {
                body: Bytes::from_static(b"answer"),
                stored_at: now,
                fresh_until: now + Duration::from_secs(30),
            },
        );
        assert!(matches!(cache.lookup(&key, now), CacheLookup::Hit(_)));
        assert!(matches!(
            cache.lookup(&key, now + Duration::from_secs(31)),
            CacheLookup::Expired
        ));
    }

    #[test]
    fn evicts_when_over_memory_limit() {
        let cache = PingolaCache::new(true, 128);
        let now = Instant::now();
        for index in 0..32u64 {
            let key = CacheKey::new(CacheNamespace::Dns, index);
            cache.insert(
                key,
                CachedValue {
                    body: Bytes::from(vec![b'x'; 32]),
                    stored_at: now,
                    fresh_until: now + Duration::from_secs(60),
                },
            );
        }
        assert!(cache.bytes() <= cache.memory_limit() as u64);
    }

    #[test]
    fn lookup_hot_path_microbench() {
        let cache = PingolaCache::new(true, 4096);
        let key = CacheKey::new(CacheNamespace::Dns, 99);
        let now = Instant::now();
        cache.insert(
            key,
            CachedValue {
                body: Bytes::from_static(b"ok"),
                stored_at: now,
                fresh_until: now + Duration::from_secs(60),
            },
        );
        let started = Instant::now();
        for _ in 0..10_000 {
            let _ = cache.lookup(&key, now);
        }
        let elapsed = started.elapsed();
        assert!(elapsed < Duration::from_secs(1));
    }
}
