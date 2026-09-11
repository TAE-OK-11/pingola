use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use bytes::Bytes;
use dashmap::DashMap;
use pingora_lru::Lru;

use crate::cache::coalesce::{CoalesceGuard, CoalescePermit};
use crate::cache::metrics::NamespaceMetrics;

const LRU_SHARDS: usize = 16;
// Approximate allocation/accounting overhead per resident entry. The body is
// still the dominant variable component, but counting only body bytes made the
// configured memory budget materially under-report real cache RSS.
const ENTRY_OVERHEAD_BYTES: usize = 128;
const MIN_ESTIMATED_ENTRY_BYTES: usize = 256;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
#[repr(u8)]
pub enum CacheNamespace {
    Dns = 1,
    Navidrome = 2,
}

impl CacheNamespace {
    fn metrics(self, cache: &PingolaCache) -> &NamespaceMetrics {
        match self {
            Self::Dns => &cache.dns_metrics,
            Self::Navidrome => &cache.navidrome_metrics,
        }
    }

    fn entries_counter(self, cache: &PingolaCache) -> &AtomicUsize {
        match self {
            Self::Dns => &cache.dns_entries,
            Self::Navidrome => &cache.navidrome_entries,
        }
    }

    fn bytes_counter(self, cache: &PingolaCache) -> &AtomicUsize {
        match self {
            Self::Dns => &cache.dns_bytes,
            Self::Navidrome => &cache.navidrome_bytes,
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

    #[inline]
    fn storage_key(self) -> u64 {
        // The first cache implementation indexed the map and LRU only by the
        // application hash. Mix the namespace into the physical key so equal
        // digests from DNS and Navidrome can never alias simply because they
        // happen to share the same digest value.
        let namespace = (self.namespace as u64)
            .wrapping_mul(0x9E37_79B9_7F4A_7C15)
            .rotate_left(17);
        self.hash ^ namespace
    }
}

#[derive(Clone)]
pub struct CachedValue {
    pub body: Bytes,
    pub stored_at: Instant,
    pub fresh_until: Instant,
}

impl CachedValue {
    fn estimated_weight(&self) -> usize {
        self.body
            .len()
            .saturating_add(ENTRY_OVERHEAD_BYTES)
            .max(1)
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
    weight: usize,
}

pub struct PingolaCache {
    enabled: bool,
    memory_limit: usize,
    data: DashMap<u64, StoredEntry>,
    order: Lru<u64, LRU_SHARDS>,
    bytes_used: AtomicUsize,
    dns_entries: AtomicUsize,
    dns_bytes: AtomicUsize,
    navidrome_entries: AtomicUsize,
    navidrome_bytes: AtomicUsize,
    dns_metrics: NamespaceMetrics,
    navidrome_metrics: NamespaceMetrics,
    coalesce: CoalesceGuard,
}

impl PingolaCache {
    pub fn new(enabled: bool, memory_bytes: usize) -> Arc<Self> {
        let memory_limit = memory_bytes.max(LRU_SHARDS * MIN_ESTIMATED_ENTRY_BYTES);
        let max_entries = (memory_limit / MIN_ESTIMATED_ENTRY_BYTES).max(LRU_SHARDS);
        let per_shard = max_entries.div_ceil(LRU_SHARDS).max(16);
        Arc::new(Self {
            enabled,
            memory_limit,
            data: DashMap::with_capacity(max_entries),
            order: Lru::with_capacity_and_watermark(
                memory_limit,
                per_shard,
                Some(max_entries),
            ),
            bytes_used: AtomicUsize::new(0),
            dns_entries: AtomicUsize::new(0),
            dns_bytes: AtomicUsize::new(0),
            navidrome_entries: AtomicUsize::new(0),
            navidrome_bytes: AtomicUsize::new(0),
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
        let storage_key = key.storage_key();
        let Some(entry) = self.data.get(&storage_key) else {
            metrics.record_miss(started.elapsed().as_nanos() as u64);
            return CacheLookup::Miss;
        };
        // The storage key already includes namespace, but retain this check as
        // an invariant guard in case the physical-key function changes later.
        if entry.namespace != key.namespace {
            metrics.record_miss(started.elapsed().as_nanos() as u64);
            return CacheLookup::Miss;
        }
        if entry.value.is_fresh(now) {
            metrics.record_hit(started.elapsed().as_nanos() as u64);
            let _ = self.order.promote(storage_key);
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
        let weight = value.estimated_weight();
        if weight > self.memory_limit {
            key.namespace.metrics(self).record_rejection();
            return false;
        }

        let storage_key = key.storage_key();
        if let Some(old) = self.data.insert(
            storage_key,
            StoredEntry {
                namespace: key.namespace,
                value,
                weight,
            },
        ) {
            self.account_remove(old.namespace, old.weight);
        }
        self.account_insert(key.namespace, weight);
        self.order.admit(storage_key, storage_key, weight);

        for (evicted_key, _) in self.order.evict_to_limit() {
            if let Some((_, entry)) = self.data.remove(&evicted_key) {
                entry.namespace.metrics(self).record_eviction(1);
                self.account_remove(entry.namespace, entry.weight);
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
        self.remove_storage_key(key.storage_key());
    }

    pub fn purge_namespace(&self, namespace: CacheNamespace) -> u64 {
        let keys: Vec<u64> = self
            .data
            .iter()
            .filter_map(|entry| (entry.namespace == namespace).then_some(*entry.key()))
            .collect();
        let mut removed = 0u64;
        for key in keys {
            if self.remove_storage_key(key) {
                removed += 1;
            }
        }
        removed
    }

    pub fn entries(&self) -> u64 {
        self.data.len() as u64
    }

    pub fn bytes(&self) -> u64 {
        self.bytes_used.load(Ordering::Relaxed) as u64
    }

    pub fn dns_metrics(&self) -> crate::cache::metrics::CacheMetricsSnapshot {
        self.dns_metrics.snapshot(
            self.dns_entries.load(Ordering::Relaxed) as u64,
            self.dns_bytes.load(Ordering::Relaxed) as u64,
        )
    }

    pub fn navidrome_metrics(&self) -> crate::cache::metrics::CacheMetricsSnapshot {
        self.navidrome_metrics.snapshot(
            self.navidrome_entries.load(Ordering::Relaxed) as u64,
            self.navidrome_bytes.load(Ordering::Relaxed) as u64,
        )
    }

    pub fn begin_fill(&self, key: CacheKey) -> CoalescePermit {
        self.coalesce
            .begin(key.storage_key(), key.namespace.metrics(self))
    }

    pub fn finish_fill(&self, permit: CoalescePermit, inserted: bool) {
        self.coalesce.finish(permit, inserted);
    }

    fn account_insert(&self, namespace: CacheNamespace, weight: usize) {
        self.bytes_used.fetch_add(weight, Ordering::Relaxed);
        namespace
            .entries_counter(self)
            .fetch_add(1, Ordering::Relaxed);
        namespace
            .bytes_counter(self)
            .fetch_add(weight, Ordering::Relaxed);
    }

    fn account_remove(&self, namespace: CacheNamespace, weight: usize) {
        self.bytes_used.fetch_sub(weight, Ordering::Relaxed);
        namespace
            .entries_counter(self)
            .fetch_sub(1, Ordering::Relaxed);
        namespace
            .bytes_counter(self)
            .fetch_sub(weight, Ordering::Relaxed);
    }

    fn remove_storage_key(&self, storage_key: u64) -> bool {
        let Some((_, entry)) = self.data.remove(&storage_key) else {
            return false;
        };
        self.account_remove(entry.namespace, entry.weight);
        let _ = self.order.remove(storage_key);
        true
    }
}

pub type CacheHandle = Arc<PingolaCache>;

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

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
    fn namespaces_do_not_alias_equal_digests() {
        let cache = PingolaCache::new(true, 4096);
        let now = Instant::now();
        let dns = CacheKey::new(CacheNamespace::Dns, 7);
        let navidrome = CacheKey::new(CacheNamespace::Navidrome, 7);
        cache.insert(
            dns,
            CachedValue {
                body: Bytes::from_static(b"dns"),
                stored_at: now,
                fresh_until: now + Duration::from_secs(60),
            },
        );
        assert!(matches!(cache.lookup(&dns, now), CacheLookup::Hit(_)));
        assert!(matches!(
            cache.lookup(&navidrome, now),
            CacheLookup::Miss
        ));
    }

    #[test]
    fn evicts_when_over_memory_limit() {
        let cache = PingolaCache::new(true, 4096);
        let now = Instant::now();
        for index in 0..128u64 {
            let key = CacheKey::new(CacheNamespace::Dns, index);
            cache.insert(
                key,
                CachedValue {
                    body: Bytes::from(vec![b'x'; 128]),
                    stored_at: now,
                    fresh_until: now + Duration::from_secs(60),
                },
            );
        }
        assert!(cache.bytes() <= cache.memory_limit() as u64);
    }

    #[test]
    fn reports_namespace_usage_separately() {
        let cache = PingolaCache::new(true, 4096);
        let now = Instant::now();
        for namespace in [CacheNamespace::Dns, CacheNamespace::Navidrome] {
            cache.insert(
                CacheKey::new(namespace, 1),
                CachedValue {
                    body: Bytes::from_static(b"ok"),
                    stored_at: now,
                    fresh_until: now + Duration::from_secs(60),
                },
            );
        }
        assert_eq!(cache.dns_metrics().entries, 1);
        assert_eq!(cache.navidrome_metrics().entries, 1);
        assert_eq!(cache.entries(), 2);
    }

    #[test]
    fn purge_namespace_does_not_touch_other_namespace() {
        let cache = PingolaCache::new(true, 4096);
        let now = Instant::now();
        let dns = CacheKey::new(CacheNamespace::Dns, 1);
        let navidrome = CacheKey::new(CacheNamespace::Navidrome, 1);
        for key in [dns, navidrome] {
            cache.insert(
                key,
                CachedValue {
                    body: Bytes::from_static(b"ok"),
                    stored_at: now,
                    fresh_until: now + Duration::from_secs(60),
                },
            );
        }
        assert_eq!(cache.purge_namespace(CacheNamespace::Navidrome), 1);
        assert!(matches!(cache.lookup(&dns, now), CacheLookup::Hit(_)));
        assert!(matches!(cache.lookup(&navidrome, now), CacheLookup::Miss));
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
