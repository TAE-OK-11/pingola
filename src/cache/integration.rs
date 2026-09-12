//! Gateway integration helpers for Pingola cache hit/miss paths.

use std::time::Instant;

use bytes::{Bytes, BytesMut};
use cloudflare_pingora::http::RequestHeader;
use http::Method;
use http::header::ACCEPT_ENCODING;

use crate::cache::core::{CacheHandle, CacheKey, CacheLookup, CacheNamespace};
use crate::cache::dns::{
    DnsCacheDecision, DnsCachePolicy, age_dns_response_for_query, build_cached_dns,
    cache_key_for_query, dns_cacheable, dns_query_key, parse_doh_query,
};
use crate::cache::navidrome::{
    NAVIDROME_CACHE_SCOPE_HEADER, NavidromeCachePolicy, build_cached_navidrome,
    navidrome_cache_key, navidrome_cacheable, navidrome_invalidates_cache,
    navidrome_response_is_error, navidrome_response_ttl,
};
use crate::config::HandlerKind;
use crate::content_encoding::negotiate;
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

/// Bounded response accumulator used while deciding whether a miss is safe to
/// cache. Once the configured limit is crossed we drop the buffered bytes and
/// stop allocating for the remainder of the response.
pub struct BoundedBody {
    bytes: BytesMut,
    limit: usize,
    overflowed: bool,
}

impl BoundedBody {
    fn new(limit: usize) -> Self {
        Self {
            bytes: BytesMut::new(),
            limit,
            overflowed: false,
        }
    }

    pub fn extend_from_slice(&mut self, chunk: &[u8]) {
        if self.overflowed {
            return;
        }
        let Some(next_len) = self.bytes.len().checked_add(chunk.len()) else {
            self.bytes.clear();
            self.overflowed = true;
            return;
        };
        if next_len > self.limit {
            self.bytes.clear();
            self.overflowed = true;
            return;
        }
        self.bytes.extend_from_slice(chunk);
    }

    fn freeze(self) -> Option<Bytes> {
        (!self.overflowed).then(|| self.bytes.freeze())
    }
}

pub struct PendingCacheInsert {
    action: PendingCacheAction,
    pub body: BoundedBody,
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

fn request_accepts_identity(request: &RequestHeader) -> bool {
    negotiate(request.headers.get_all(ACCEPT_ENCODING).iter()).identity_acceptable
}

fn request_cache_scope(request: &RequestHeader) -> Option<&str> {
    request
        .headers
        .get(NAVIDROME_CACHE_SCOPE_HEADER)
        .and_then(|value| value.to_str().ok())
}

fn prepare_navidrome_lookup(
    cache: &CacheRuntime,
    request: &RequestHeader,
    now: Instant,
) -> PreparedCacheLookup {
    if !cache.navidrome.enabled || !request_accepts_identity(request) {
        return PreparedCacheLookup::Bypass;
    }
    let cacheable = match navidrome_cacheable(
        &request.method,
        request.uri.path(),
        request.uri.query(),
        request_cache_scope(request),
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
                body: BoundedBody::new(cache.dns.max_response_bytes),
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
            let cacheable = request_accepts_identity(request)
                .then(|| {
                    navidrome_cacheable(
                        &request.method,
                        request.uri.path(),
                        request.uri.query(),
                        request_cache_scope(request),
                        &cache.navidrome,
                    )
                })
                .flatten();
            let action = if let Some(cacheable) = cacheable {
                PendingCacheAction::Store(navidrome_cache_key(&cacheable))
            } else if navidrome_invalidates_cache(request.uri.path()) {
                PendingCacheAction::Invalidate(CacheNamespace::Navidrome)
            } else {
                return None;
            };
            Some(PendingCacheInsert {
                action,
                body: BoundedBody::new(cache.navidrome.max_response_bytes),
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
    if pending.body.overflowed {
        if let PendingCacheAction::Store(key) = &pending.action {
            cache.store.reject(key.namespace);
        }
        return false;
    }
    let Some(body) = pending.body.freeze() else {
        return false;
    };
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
                cache.store.insert(key, build_cached_dns(&body, ttl, now))
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
                if !is_json || serde_json::from_slice::<serde_json::Value>(&body).is_err() {
                    // A compressed response body cannot be replayed safely because
                    // cached responses intentionally do not preserve Content-Encoding.
                    // Strict JSON validation also rejects malformed/JSONP payloads.
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

    const CACHE_SCOPE_A: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const CACHE_SCOPE_B: &str = "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";

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
    fn dns_hit_materializer_echoes_caller_transaction_id() {
        let store = PingolaCache::new(true, 4096);
        let now = Instant::now();
        let mut response = vec![
            0x00, 0x00, 0x81, 0x80, 0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 7, b'e', b'x',
            b'a', b'm', b'p', b'l', b'e', 3, b'c', b'o', b'm', 0, 0, 1, 0, 1,
        ];
        response.extend_from_slice(&[0xC0, 0x0C, 0x00, 0x01, 0x00, 0x01]);
        response.extend_from_slice(&120u32.to_be_bytes());
        response.extend_from_slice(&4u16.to_be_bytes());
        response.extend_from_slice(&[192, 0, 2, 1]);

        let mut query = response[..31].to_vec();
        query[0..2].copy_from_slice(&0x1234u16.to_be_bytes());
        query[2] = 0x01;
        query[3] = 0x00;
        query[6] = 0;
        query[7] = 0;

        let key = cache_key_for_query(&dns_query_key(&query).unwrap());
        assert!(store.insert(
            key,
            build_cached_dns(&response, std::time::Duration::from_secs(120), now),
        ));
        let cache = CacheRuntime {
            store: store.clone(),
            dns: DnsCachePolicy::default(),
            navidrome: NavidromeCachePolicy::default(),
        };
        let request = RequestHeader::build(Method::POST, b"/dns-query", None).unwrap();
        match prepare_lookup(
            &cache,
            RouteClass::Doh,
            HandlerKind::AdguardDns,
            &request,
            Some(&query),
            now,
        ) {
            PreparedCacheLookup::Hit(hit) => assert_eq!(&hit.body[0..2], &[0x12, 0x34]),
            PreparedCacheLookup::Miss(_) => panic!("expected dns cache hit"),
            PreparedCacheLookup::Bypass => panic!("expected dns cache hit, got bypass"),
        }

        let value = match store.lookup(&key, now) {
            CacheLookup::Hit(value) => value,
            _ => panic!("missing cached dns value"),
        };
        let coalesced =
            age_dns_response_for_query(&value.body, value.stored_at, now, 0x99AA).unwrap();
        assert_eq!(&coalesced[0..2], &[0x99, 0xAA]);
    }

    #[test]
    fn navidrome_cache_bypasses_when_identity_is_forbidden() {
        let store = PingolaCache::new(true, 4096);
        let cache = CacheRuntime {
            store,
            dns: DnsCachePolicy::default(),
            navidrome: NavidromeCachePolicy::default(),
        };
        let mut request = RequestHeader::build(
            Method::GET,
            b"/rest/getAlbum.view?u=alice&p=secret&id=1&f=json",
            None,
        )
        .unwrap();
        request
            .insert_header(ACCEPT_ENCODING, "gzip, identity;q=0")
            .unwrap();
        assert!(matches!(
            prepare_lookup(
                &cache,
                RouteClass::NavidromeApi,
                HandlerKind::NavidromeMain,
                &request,
                None,
                Instant::now(),
            ),
            PreparedCacheLookup::Bypass
        ));
    }

    #[test]
    fn stable_scope_hits_across_rotating_token_salt() {
        let store = PingolaCache::new(true, 4096);
        let cache = CacheRuntime {
            store,
            dns: DnsCachePolicy::default(),
            navidrome: NavidromeCachePolicy::default(),
        };
        let now = Instant::now();
        let mut first = RequestHeader::build(
            Method::GET,
            b"/rest/getAlbum.view?u=alice&t=one&s=salt1&id=1&f=json",
            None,
        )
        .unwrap();
        first
            .insert_header(NAVIDROME_CACHE_SCOPE_HEADER, CACHE_SCOPE_A)
            .unwrap();
        let mut pending = begin_pending_insert(
            &cache,
            RouteClass::NavidromeApi,
            HandlerKind::NavidromeMain,
            &first,
            None,
        )
        .unwrap();
        pending.content_type = Some(JSON_CONTENT_TYPE.clone());
        pending
            .body
            .extend_from_slice(br#"{"subsonic-response":{"status":"ok"}}"#);
        assert!(store_pending_insert(&cache, pending, now));

        let mut rotated = RequestHeader::build(
            Method::GET,
            b"/rest/getAlbum.view?u=alice&t=two&s=salt2&id=1&f=json",
            None,
        )
        .unwrap();
        rotated
            .insert_header(NAVIDROME_CACHE_SCOPE_HEADER, CACHE_SCOPE_A)
            .unwrap();
        assert!(matches!(
            prepare_lookup(
                &cache,
                RouteClass::NavidromeApi,
                HandlerKind::NavidromeMain,
                &rotated,
                None,
                now,
            ),
            PreparedCacheLookup::Hit(_)
        ));

        rotated
            .insert_header(NAVIDROME_CACHE_SCOPE_HEADER, CACHE_SCOPE_B)
            .unwrap();
        assert!(matches!(
            prepare_lookup(
                &cache,
                RouteClass::NavidromeApi,
                HandlerKind::NavidromeMain,
                &rotated,
                None,
                now,
            ),
            PreparedCacheLookup::Miss(_)
        ));
    }

    #[test]
    fn navidrome_encoded_or_malformed_body_is_not_cached() {
        let store = PingolaCache::new(true, 4096);
        let cache = CacheRuntime {
            store,
            dns: DnsCachePolicy::default(),
            navidrome: NavidromeCachePolicy::default(),
        };
        let request = RequestHeader::build(
            Method::GET,
            b"/rest/getAlbum.view?u=alice&p=secret&id=1&f=json",
            None,
        )
        .unwrap();
        let mut pending = begin_pending_insert(
            &cache,
            RouteClass::NavidromeApi,
            HandlerKind::NavidromeMain,
            &request,
            None,
        )
        .unwrap();
        pending.content_type = Some(JSON_CONTENT_TYPE.clone());
        pending.body.extend_from_slice(&[0x1f, 0x8b, 0x08, 0x00]);
        assert!(!store_pending_insert(&cache, pending, Instant::now()));
    }

    #[test]
    fn oversized_pending_body_stops_buffering() {
        let store = PingolaCache::new(true, 4096);
        let mut navidrome = NavidromeCachePolicy::default();
        navidrome.max_response_bytes = 8;
        let cache = CacheRuntime {
            store,
            dns: DnsCachePolicy::default(),
            navidrome,
        };
        let request = RequestHeader::build(
            Method::GET,
            b"/rest/getAlbum.view?u=alice&p=secret&id=1&f=json",
            None,
        )
        .unwrap();
        let mut pending = begin_pending_insert(
            &cache,
            RouteClass::NavidromeApi,
            HandlerKind::NavidromeMain,
            &request,
            None,
        )
        .unwrap();
        pending.body.extend_from_slice(b"12345678");
        assert_eq!(pending.body.bytes.len(), 8);
        pending.body.extend_from_slice(b"9");
        assert!(pending.body.overflowed);
        assert!(pending.body.bytes.is_empty());
        pending.body.extend_from_slice(b"more ignored bytes");
        assert!(pending.body.bytes.is_empty());
        assert!(!store_pending_insert(&cache, pending, Instant::now()));
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
