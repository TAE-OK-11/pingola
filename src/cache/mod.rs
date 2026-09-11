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

pub use coalesce::CoalescePermit;
pub use core::{CacheKey, CacheLookup, CachedValue, PingolaCache};
#[allow(unused_imports)] // age_dns_response remains part of the public cache helpers
pub use dns::{DnsCachePolicy, age_dns_response, age_dns_response_for_query, parse_doh_query};
pub use integration::{
    CacheRuntime, CachedHttpResponse, PendingCacheInsert, PreparedCacheLookup,
    begin_pending_insert, dns_request_needs_body, prepare_lookup, store_pending_insert,
};
pub use navidrome::NavidromeCachePolicy;
