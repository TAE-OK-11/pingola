//! Gateway integration helpers for Pingola cache hit/miss paths.

use std::time::Instant;

use bytes::{Bytes, BytesMut};
use cloudflare_pingora::http::RequestHeader;
use http::Method;

use crate::cache::core::{CacheHandle, CacheKey, CacheLookup, CacheNamespace};
use crate::cache::dns::{
    DnsCacheDecision, DnsCachePolicy, age_dns_response_for_query, build_cached_dns,
    cache_key_for_query, dns_cacheable, dns_query_key, parse_doh_query,
};
use crate::cache::navidrome::{
    NavidromeCachePolicy, build_cached_navidrome, navidrome_cache_key, navidrome_cacheable,
    navidrome_invalidates_cache, navidrome_response_is_error, navidrome_response_ttl,
};
use crate::config::HandlerKind;
use crate::routing::RouteClass;

pub struct CacheRuntime {
    pub store: CacheHandle,
    pub dns: DnsCachePolicy,
    pub navidrome: NavidromeCachePolicy,
}

pub enum PreparedCacheLookup {
    Miss(CacheKey),
    Hit(CachedHttpResponse),
    Bypass,
}

#[derive(Clone)]
pub struct CachedHttpResponse {
    pub status: u16,
    pub content_type: http::header::HeaderValue,
    pub body: Bytes,
}

const DNS_CONTENT_TYPE: http::header::HeaderValue =
    http::header::HeaderValue::from_static("application/dns-message");
const JSON_CONTENT_TYPE: http::header::HeaderValue =
    http::header::HeaderValue::from_static("application/json");

enum PendingCacheAction {
    Store(CacheKey),
    Invalidate(CacheNamespace),
}

pub struct PendingCacheInsert {
    action: PendingCacheAction,
    pub body: BytesMut,
    pub status: u16,
    pub content_type: Option<http::header::HeaderValue>,
}

pub fn prepare_lookup(
    cache: &CacheRuntime,
    route: RouteClass,
    handler: HandlerKind,
    request: &RequestHeader,
    dns_body: Option<&[u8]>,
    now: Instant,
) -> PreparedCacheLookup {
    if !cache.store.enabled() {
        return PreparedCacheLookup::Bypass;
    }
    match route {
        RouteClass::Doh => prepare_dns_lookup(cache, request, dns_body, now),
        RouteClass::NavidromeApi
            if matches!(
                handler,
                HandlerKind::NavidromeMain | HandlerKind::NavidromeCdn
            ) =>
        {
            prepare_navidrome_lookup(cache, request, now)
        }
        _ => PreparedCacheLookup::Bypass,
    }
}

fn prepare_dns_lookup(
    cache: &CacheRuntime,
    request: &RequestHeader,
    dns_body: Option<&[u8]>,
    now: Instant,
) -> PreparedCacheLookup {
    if !cache.dns.enabled {
        return PreparedCacheLookup::Bypass;
    }
    let path = request
        .uri
        .path_and_query()
        .map_or("/", |value| value.as_str());
    let wire = match dns_body
        .map(|body| body.to_vec())
        .or_else(|| parse_doh_query(request.method.as_str(), path, &[]))
    {
        Some(wire) => wire,
        None => return PreparedCacheLookup::Bypass,
    };
    let query = match dns_query_key(&wire) {
        Some(query) => query,
        None => return PreparedCacheLookup::Bypass,
    };
    let query_id = u16::from_be_bytes([wire[0], wire[1]]);
    let key = cache_key_for_query(&query);
    match cache.store.lookup(&key, now) {
        CacheLookup::Hit(value) => {
            let body = age_dns_response_for_query(&value.body, value.stored_at, now, query_id);
            body.map(|body| {
                PreparedCacheLookup::Hit(CachedHttpResponse {
                    status: 200,
                    content_type: DNS_CONTENT_TYPE.clone(),
                    body,
                })
            })
            .unwrap_or(PreparedCacheLookup::Miss(key))
        }
        CacheLookup::Miss | CacheLookup::Expired => PreparedCacheLookup::Miss(key),
    }
}

fn prepare_navidrome_lookup(
    cache: &CacheRuntime,
    request: &RequestHeader,
    now: Instant,
) -> PreparedCacheLookup {
    if !cache.navidrome.enabled {
        return PreparedCacheLookup::Bypass;
    }
    let cacheable = match navidrome_cacheable(
        &request.method,
        request.uri.path(),
        request.uri.query(),
        &cache.navidrome,
    ) {
        Some(cacheable) => cacheable,
        None => return PreparedCacheLookup::Bypass,
    };
    let key = navidrome_cache_key(&cacheable);
    match cache.store.lookup(&key, now) {
        CacheLookup::Hit(value) => PreparedCacheLookup::Hit(CachedHttpResponse {
            status: 200,
            content_type: JSON_CONTENT_TYPE.clone(),
            body: value.body,
        }),
        CacheLookup::Miss | CacheLookup::Expired => PreparedCacheLookup::Miss(key),
    }
}

pub fn begin_pending_insert(
    cache: &CacheRuntime,
    route: RouteClass,
    handler: HandlerKind,
    request: &RequestHeader,
    dns_body: Option<&[u8]>,
) -> Option<PendingCacheInsert> {
    if !cache.store.enabled() {
        return None;
    }
    match route {
        RouteClass::Doh => {
            if !cache.dns.enabled {
                return None;
            }
            let path = request
                .uri
                .path_and_query()
                .map_or("/", |value| value.as_str());
            let wire = dns_body
                .map(|body| body.to_vec())
                .or_else(|| parse_doh_query(request.method.as_str(), path, &[]))?;
            let query = dns_query_key(&wire)?;
            Some(PendingCacheInsert {
                action: PendingCacheAction::Store(cache_key_for_query(&query)),
                body: BytesMut::new(),
                status: 200,
                content_type: Some(DNS_CONTENT_TYPE.clone()),
            })
        }
        RouteClass::NavidromeApi
            if matches!(
                handler,
                HandlerKind::NavidromeMain | HandlerKind::NavidromeCdn
            ) =>
        {
            if !cache.navidrome.enabled {
                return None;
            }
            let action = if let Some(cacheable) = navidrome_cacheable(
                &request.method,
                request.uri.path(),
                request.uri.query(),
                &cache.navidrome,
            ) {
                PendingCacheAction::Store(navidrome_cache_key(&cacheable))
            } else if navidrome_invalidates_cache(request.uri.path()) {
                PendingCacheAction::Invalidate(CacheNamespace::Navidrome)
            } else {
                return None;
            };
            Some(PendingCacheInsert {
                action,
                body: BytesMut::new(),
                status: 200,
                content_type: None,
            })
        }
        _ => None,
    }
}

pub fn store_pending_insert(
    cache: &CacheRuntime,
    pending: PendingCacheInsert,
    now: Instant,
) -> bool {
    let body = pending.body.freeze();
    match pending.action {
        PendingCacheAction::Invalidate(CacheNamespace::Navidrome) => {
            // OpenSubsonic often reports application errors with HTTP 200. Only
            // invalidate after a response that is not an API-level error.
            if navidrome_response_is_error(&body) {
                return false;
            }
            cache.store.purge_namespace(CacheNamespace::Navidrome);
            true
        }
        PendingCacheAction::Invalidate(CacheNamespace::Dns) => false,
        PendingCacheAction::Store(key) => match key.namespace {
            CacheNamespace::Dns => {
                let decision = dns_cacheable(&body, &cache.dns);
                let DnsCacheDecision::Cacheable { ttl } = decision else {
                    cache.store.reject(key.namespace);
                    return false;
                };
                cache
                    .store
                    .insert(key, build_cached_dns(&body, ttl, now))
            }
            CacheNamespace::Navidrome => {
                let is_json = pending
                    .content_type
                    .as_ref()
                    .and_then(|value| value.to_str().ok())
                    .is_some_and(|value| {
                        value.split(';').next().is_some_and(|mime| {
                            mime.trim().eq_ignore_ascii_case("application/json")
                        })
                    });
                if !is_json {
                    cache.store.reject(key.namespace);
                    return false;
                }
                let ttl = navidrome_response_ttl(&body, None, &cache.navidrome);
                let Some(ttl) = ttl else {
                    cache.store.reject(key.namespace);
                    return false;
                };
                cache
                    .store
                    .insert(key, build_cached_navidrome(body, ttl, now))
            }
        },
    }
}

pub fn dns_request_needs_body(method: &Method) -> bool {
    *method == Method::POST
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::core::PingolaCache;

    #[test]
    fn dns_lookup_miss_then_hit() {
        let store = PingolaCache::new(true, 4096);
        let cache = CacheRuntime {
            store: store.clone(),
            dns: DnsCachePolicy::default(),
            navidrome: NavidromeCachePolicy::default(),
        };
        let request = RequestHeader::build(Method::GET, b"/dns-query", None).unwrap();
        let wire = vec![
            0x12, 0x34, 0x01, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 7, b'e', b'x',
            b'a', b'm', b'p', b'l', b'e', 3, b'c', b'o', b'm', 0, 0, 1, 0, 1,
        ];
        let now = Instant::now();
        assert!(matches!(
            prepare_lookup(
                &cache,
                RouteClass::Doh,
                HandlerKind::AdguardDns,
                &request,
                Some(&wire),
                now
            ),
            PreparedCacheLookup::Miss(_)
        ));
    }

    #[test]
    fn successful_mutation_purges_only_navidrome_namespace() {
        let store = PingolaCache::new(true, 4096);
        let now = Instant::now();
        let dns_key = CacheKey::new(CacheNamespace::Dns, 1);
        let nav_key = CacheKey::new(CacheNamespace::Navidrome, 1);
        for key in [dns_key, nav_key] {
            store.insert(
                key,
                crate::cache::core::CachedValue {
                    body: Bytes::from_static(b"ok"),
                    stored_at: now,
                    fresh_until: now + std::time::Duration::from_secs(60),
                },
            );
        }
        let cache = CacheRuntime {
            store: store.clone(),
            dns: DnsCachePolicy::default(),
            navidrome: NavidromeCachePolicy::default(),
        };
        let request = RequestHeader::build(
            Method::GET,
            b"/rest/star.view?u=alice&p=secret&id=1&f=json",
            None,
        )
        .unwrap();
        let pending = begin_pending_insert(
            &cache,
            RouteClass::NavidromeApi,
            HandlerKind::NavidromeMain,
            &request,
            None,
        )
        .unwrap();
        assert!(store_pending_insert(&cache, pending, now));
        assert!(matches!(store.lookup(&dns_key, now), CacheLookup::Hit(_)));
        assert!(matches!(store.lookup(&nav_key, now), CacheLookup::Miss));
    }
}
