use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::json;

#[derive(Default)]
pub struct NamespaceMetrics {
    pub hits: AtomicU64,
    pub misses: AtomicU64,
    pub inserts: AtomicU64,
    pub rejections: AtomicU64,
    pub expirations: AtomicU64,
    pub evictions: AtomicU64,
    pub coalesced: AtomicU64,
    pub lookup_ns: AtomicU64,
}

impl NamespaceMetrics {
    pub fn record_hit(&self, lookup_ns: u64) {
        self.hits.fetch_add(1, Ordering::Relaxed);
        self.lookup_ns.fetch_add(lookup_ns, Ordering::Relaxed);
    }

    pub fn record_miss(&self, lookup_ns: u64) {
        self.misses.fetch_add(1, Ordering::Relaxed);
        self.lookup_ns.fetch_add(lookup_ns, Ordering::Relaxed);
    }

    pub fn record_insert(&self) {
        self.inserts.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_rejection(&self) {
        self.rejections.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_expiration(&self) {
        self.expirations.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_eviction(&self, count: u64) {
        self.evictions.fetch_add(count, Ordering::Relaxed);
    }

    pub fn record_coalesced(&self) {
        self.coalesced.fetch_add(1, Ordering::Relaxed);
    }

    pub fn snapshot(&self, entries: u64, bytes: u64) -> CacheMetricsSnapshot {
        let hits = self.hits.load(Ordering::Relaxed);
        let misses = self.misses.load(Ordering::Relaxed);
        let lookups = hits + misses;
        CacheMetricsSnapshot {
            hits,
            misses,
            hit_ratio: if lookups == 0 {
                0.0
            } else {
                hits as f64 / lookups as f64
            },
            entries,
            bytes,
            inserts: self.inserts.load(Ordering::Relaxed),
            rejections: self.rejections.load(Ordering::Relaxed),
            expirations: self.expirations.load(Ordering::Relaxed),
            evictions: self.evictions.load(Ordering::Relaxed),
            coalesced: self.coalesced.load(Ordering::Relaxed),
            lookup_ns_total: self.lookup_ns.load(Ordering::Relaxed),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CacheMetricsSnapshot {
    pub hits: u64,
    pub misses: u64,
    pub hit_ratio: f64,
    pub entries: u64,
    pub bytes: u64,
    pub inserts: u64,
    pub rejections: u64,
    pub expirations: u64,
    pub evictions: u64,
    pub coalesced: u64,
    pub lookup_ns_total: u64,
}

impl CacheMetricsSnapshot {
    pub fn to_json(&self) -> serde_json::Value {
        json!({
            "hits": self.hits,
            "misses": self.misses,
            "hit_ratio": self.hit_ratio,
            "entries": self.entries,
            "bytes": self.bytes,
            "inserts": self.inserts,
            "rejections": self.rejections,
            "expirations": self.expirations,
            "evictions": self.evictions,
            "coalesced": self.coalesced,
            "lookup_ns_total": self.lookup_ns_total,
        })
    }
}
