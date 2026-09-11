//! Pingola in-memory cache foundation.
//!
//! A thin abstraction over [`pingora_lru::Lru`] with namespace-aware keys, TTL
//! expiration, request coalescing, and lightweight metrics. DNS and Navidrome
//! namespaces build on this core without coupling callers to Pingora HTTP cache APIs.

mod coalesce;
mod core;
mod dns;
mod integration;
mod metrics;
mod navidrome;

pub use coalesce::{CoalesceGuard, CoalescePermit};
pub use core::{CacheHandle, CacheKey, CacheLookup, CacheNamespace, CachedValue, PingolaCache};
pub use dns::{DnsCachePolicy, DnsQueryKey, age_dns_response, dns_cacheable, parse_doh_query};
pub use integration::{
    CacheRuntime, CachedHttpResponse, PendingCacheInsert, PreparedCacheLookup,
    begin_pending_insert, dns_request_needs_body, prepare_lookup, store_pending_insert,
};
pub use metrics::{CacheMetricsSnapshot, NamespaceMetrics};
pub use navidrome::{
    NavidromeCachePolicy, NavidromeCacheable, navidrome_cache_key, navidrome_cacheable,
};
