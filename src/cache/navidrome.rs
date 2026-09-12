//! Conservative Navidrome / OpenSubsonic metadata cache helpers.

use std::hash::Hasher;
use std::time::{Duration, Instant};

use ahash::AHasher;
use bytes::Bytes;
use http::Method;

use crate::cache::core::{CacheKey, CacheNamespace, CachedValue};

pub const NAVIDROME_CACHE_SCOPE_HEADER: &str = "x-bufi-cache-scope";

const CACHEABLE_SUFFIXES: &[&str] = &[
    "getAlbum",
    "getArtist",
    "getSong",
    "getGenres",
    "getMusicFolders",
    "getAlbumInfo",
    "getArtistInfo",
    "getSongInfo",
    "getLyricsBySongId",
    "getLyrics",
];

const BLOCKED_SUFFIXES: &[&str] = &[
    "scrobble",
    "star",
    "unstar",
    "setRating",
    "savePlayQueue",
    "create",
    "update",
    "delete",
    "search",
    "getStarred",
    "getRandomSongs",
    "getNowPlaying",
    "getPlaylists",
    "getPlaylist",
    "getUser",
    "getUsers",
    "getCoverArt",
    "download",
    "stream",
];

// These operations directly change user-specific fields present in cached
// getSong/getAlbum/getArtist payloads. Purge the small metadata namespace only
// after a confirmed successful response. Scrobbles intentionally do not purge:
// they are frequent and play-count freshness can safely follow the short TTL.
const INVALIDATING_SUFFIXES: &[&str] = &["star", "unstar", "setRating"];

#[derive(Clone, Copy, Debug)]
pub struct NavidromeCachePolicy {
    pub enabled: bool,
    pub default_ttl: Duration,
    pub max_ttl: Duration,
    pub max_response_bytes: usize,
}

impl Default for NavidromeCachePolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            default_ttl: Duration::from_secs(60),
            max_ttl: Duration::from_secs(300),
            max_response_bytes: 512 * 1024,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NavidromeCacheable {
    pub endpoint: String,
    pub user_scope: String,
    pub stable_params: Vec<(String, String)>,
}

pub fn navidrome_cacheable(
    method: &Method,
    path: &str,
    query: Option<&str>,
    client_cache_scope: Option<&str>,
    policy: &NavidromeCachePolicy,
) -> Option<NavidromeCacheable> {
    if !policy.enabled || *method != Method::GET {
        return None;
    }

    let endpoint = normalize_endpoint(path)?;
    if BLOCKED_SUFFIXES
        .iter()
        .any(|blocked| endpoint.eq_ignore_ascii_case(blocked))
    {
        return None;
    }
    if !CACHEABLE_SUFFIXES
        .iter()
        .any(|allowed| endpoint.eq_ignore_ascii_case(allowed))
    {
        return None;
    }

    let params = parse_query_pairs(query);

    // Cache only JSON responses in this first implementation. Serving a cached
    // XML/JSONP body with a hard-coded JSON content type would be incorrect, and
    // callback responses are executable content that should never be shared by
    // this cache layer.
    if params.iter().any(|(key, _)| key == "callback") {
        return None;
    }
    let response_format = params.iter().find(|(key, _)| key == "f")?.1.as_str();
    if !response_format.eq_ignore_ascii_case("json") {
        return None;
    }

    // A username alone is not authentication. Normal clients remain scoped by
    // their authentication secret/proof. BuFi password auth intentionally uses
    // a fresh t/s pair per request; a high-entropy per-process cache capability
    // lets those requests share a cache key without weakening the auth check for
    // clients that do not opt in. The capability is never a password derivative.
    let user_scope = authenticated_user_scope(&params, client_cache_scope)?;
    let stable_params = stable_query_params(&params);

    Some(NavidromeCacheable {
        endpoint,
        user_scope,
        stable_params,
    })
}

pub fn navidrome_invalidates_cache(path: &str) -> bool {
    normalize_endpoint(path).is_some_and(|endpoint| {
        INVALIDATING_SUFFIXES
            .iter()
            .any(|candidate| endpoint.eq_ignore_ascii_case(candidate))
    })
}

pub fn navidrome_cache_key(entry: &NavidromeCacheable) -> CacheKey {
    let mut hasher = AHasher::default();
    hasher.write(entry.endpoint.as_bytes());
    hasher.write(entry.user_scope.as_bytes());
    for (key, value) in &entry.stable_params {
        hasher.write(key.as_bytes());
        hasher.write_u8(0);
        hasher.write(value.as_bytes());
        hasher.write_u8(0xff);
    }
    CacheKey::new(CacheNamespace::Navidrome, hasher.finish())
}

pub fn build_cached_navidrome(body: Bytes, ttl: Duration, now: Instant) -> CachedValue {
    CachedValue {
        body,
        stored_at: now,
        fresh_until: now + ttl,
    }
}

pub fn navidrome_response_ttl(
    response_body: &[u8],
    cache_control_max_age: Option<u64>,
    policy: &NavidromeCachePolicy,
) -> Option<Duration> {
    if response_body.len() > policy.max_response_bytes || navidrome_response_is_error(response_body)
    {
        return None;
    }
    let ttl = cache_control_max_age
        .map(Duration::from_secs)
        .unwrap_or(policy.default_ttl);
    Some(ttl.min(policy.max_ttl))
}

pub fn navidrome_response_is_error(body: &[u8]) -> bool {
    let prefix = &body[..body.len().min(512)];
    let text = String::from_utf8_lossy(prefix);
    text.contains("<error ")
        || text.contains("\"status\":\"failed\"")
        || text.contains("\"status\": \"failed\"")
}

fn normalize_endpoint(path: &str) -> Option<String> {
    let trimmed = path.trim_start_matches('/');
    let base = trimmed
        .strip_prefix("rest/")
        .unwrap_or(trimmed)
        .split('/')
        .next()?;
    let without_view = base.strip_suffix(".view").unwrap_or(base);
    if without_view.is_empty() {
        None
    } else {
        Some(without_view.to_string())
    }
}

fn parse_query_pairs(query: Option<&str>) -> Vec<(String, String)> {
    let Some(query) = query else {
        return Vec::new();
    };
    let mut pairs = Vec::new();
    for pair in query.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        pairs.push((key.to_string(), value.to_string()));
    }
    pairs.sort();
    pairs
}

fn valid_client_cache_scope(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn authenticated_user_scope(
    params: &[(String, String)],
    client_cache_scope: Option<&str>,
) -> Option<String> {
    let username = params
        .iter()
        .find(|(key, _)| key == "u")
        .map(|(_, value)| value.as_str())
        .unwrap_or("");

    let mut hasher = AHasher::default();
    hasher.write(username.as_bytes());
    hasher.write_u8(0);

    if let Some((_, api_key)) = params.iter().find(|(key, _)| key == "apiKey") {
        if api_key.is_empty() {
            return None;
        }
        hasher.write(b"apiKey\0");
        hasher.write(api_key.as_bytes());
    } else if let Some((_, password)) = params.iter().find(|(key, _)| key == "p") {
        if username.is_empty() || password.is_empty() {
            return None;
        }
        hasher.write(b"password\0");
        hasher.write(password.as_bytes());
    } else if let Some((_, jwt)) = params.iter().find(|(key, _)| key == "jwt") {
        if jwt.is_empty() {
            return None;
        }
        hasher.write(b"jwt\0");
        hasher.write(jwt.as_bytes());
    } else {
        let token = params.iter().find(|(key, _)| key == "t")?.1.as_str();
        let salt = params.iter().find(|(key, _)| key == "s")?.1.as_str();
        if username.is_empty() || token.is_empty() || salt.is_empty() {
            return None;
        }
        if let Some(scope) = client_cache_scope.filter(|scope| valid_client_cache_scope(scope)) {
            hasher.write(b"client-cache-scope\0");
            for byte in scope.bytes() {
                hasher.write_u8(byte.to_ascii_lowercase());
            }
        } else {
            // Safe fallback for every existing client: rotating t/s values still
            // isolate cache entries exactly as before when no capability exists.
            hasher.write(b"token-salt\0");
            hasher.write(token.as_bytes());
            hasher.write_u8(0);
            hasher.write(salt.as_bytes());
        }
    }

    Some(format!("auth:{:016x}", hasher.finish()))
}

fn stable_query_params(params: &[(String, String)]) -> Vec<(String, String)> {
    const SENSITIVE: &[&str] = &["u", "t", "s", "p", "apiKey", "jwt"];

    params
        .iter()
        .filter(|(key, _)| {
            !SENSITIVE.contains(&key.as_str())
                && key != "callback"
                && !key.ends_with("Token")
                && !key.ends_with("Salt")
        })
        // Keep every non-secret request parameter in the key. This is slightly
        // more conservative for hit rate, but prevents version/format/client or
        // future endpoint parameters from aliasing distinct responses.
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const CACHE_SCOPE_A: &str =
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const CACHE_SCOPE_B: &str =
        "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";

    #[test]
    fn only_allows_conservative_authenticated_json_endpoints() {
        let policy = NavidromeCachePolicy::default();
        assert!(
            navidrome_cacheable(
                &Method::GET,
                "/rest/getAlbum.view",
                Some("u=alice&t=token&s=salt&id=1&f=json"),
                None,
                &policy
            )
            .is_some()
        );
        assert!(
            navidrome_cacheable(
                &Method::GET,
                "/rest/scrobble.view",
                Some("u=alice&t=token&s=salt&id=1&f=json"),
                None,
                &policy
            )
            .is_none()
        );
    }

    #[test]
    fn lyrics_are_cacheable_authenticated_json_reads() {
        let policy = NavidromeCachePolicy::default();
        for endpoint in ["getLyricsBySongId", "getLyrics"] {
            assert!(
                navidrome_cacheable(
                    &Method::GET,
                    &format!("/rest/{endpoint}.view"),
                    Some("u=alice&t=token&s=salt&id=1&f=json"),
                    None,
                    &policy,
                )
                .is_some(),
                "endpoint={endpoint}"
            );
        }
    }

    #[test]
    fn username_without_auth_never_hits_cache() {
        let policy = NavidromeCachePolicy::default();
        assert!(
            navidrome_cacheable(
                &Method::GET,
                "/rest/getAlbum.view",
                Some("u=alice&id=1&f=json"),
                Some(CACHE_SCOPE_A),
                &policy,
            )
            .is_none()
        );
    }

    #[test]
    fn auth_proof_is_part_of_user_scope_without_client_capability() {
        let policy = NavidromeCachePolicy::default();
        let first = navidrome_cacheable(
            &Method::GET,
            "/rest/getSong.view",
            Some("u=alice&id=9&t=one&s=salt1&f=json"),
            None,
            &policy,
        )
        .unwrap();
        let second = navidrome_cacheable(
            &Method::GET,
            "/rest/getSong.view",
            Some("u=alice&id=9&t=two&s=salt2&f=json"),
            None,
            &policy,
        )
        .unwrap();
        assert_ne!(navidrome_cache_key(&first), navidrome_cache_key(&second));
    }

    #[test]
    fn stable_client_scope_reuses_key_across_rotating_token_salt() {
        let policy = NavidromeCachePolicy::default();
        let first = navidrome_cacheable(
            &Method::GET,
            "/rest/getAlbum.view",
            Some("u=alice&t=one&s=salt1&id=1&f=json"),
            Some(CACHE_SCOPE_A),
            &policy,
        )
        .unwrap();
        let second = navidrome_cacheable(
            &Method::GET,
            "/rest/getAlbum.view",
            Some("u=alice&t=two&s=salt2&id=1&f=json"),
            Some(CACHE_SCOPE_A),
            &policy,
        )
        .unwrap();
        assert_eq!(navidrome_cache_key(&first), navidrome_cache_key(&second));
    }

    #[test]
    fn client_scope_is_a_real_capability_not_a_username_alias() {
        let policy = NavidromeCachePolicy::default();
        let first = navidrome_cacheable(
            &Method::GET,
            "/rest/getAlbum.view",
            Some("u=alice&t=one&s=salt1&id=1&f=json"),
            Some(CACHE_SCOPE_A),
            &policy,
        )
        .unwrap();
        let different_scope = navidrome_cacheable(
            &Method::GET,
            "/rest/getAlbum.view",
            Some("u=alice&t=two&s=salt2&id=1&f=json"),
            Some(CACHE_SCOPE_B),
            &policy,
        )
        .unwrap();
        let invalid_scope = navidrome_cacheable(
            &Method::GET,
            "/rest/getAlbum.view",
            Some("u=alice&t=two&s=salt2&id=1&f=json"),
            Some("not-a-valid-cache-scope"),
            &policy,
        )
        .unwrap();
        assert_ne!(
            navidrome_cache_key(&first),
            navidrome_cache_key(&different_scope)
        );
        assert_ne!(navidrome_cache_key(&first), navidrome_cache_key(&invalid_scope));
    }

    #[test]
    fn users_are_isolated_even_with_same_password_material() {
        let policy = NavidromeCachePolicy::default();
        let alice = navidrome_cacheable(
            &Method::GET,
            "/rest/getAlbum.view",
            Some("u=alice&p=secret&id=1&f=json"),
            None,
            &policy,
        )
        .unwrap();
        let bob = navidrome_cacheable(
            &Method::GET,
            "/rest/getAlbum.view",
            Some("u=bob&p=secret&id=1&f=json"),
            None,
            &policy,
        )
        .unwrap();
        assert_ne!(navidrome_cache_key(&alice), navidrome_cache_key(&bob));
    }

    #[test]
    fn non_json_and_jsonp_are_not_cacheable() {
        let policy = NavidromeCachePolicy::default();
        assert!(
            navidrome_cacheable(
                &Method::GET,
                "/rest/getSong.view",
                Some("u=alice&p=secret&id=1&f=xml"),
                None,
                &policy,
            )
            .is_none()
        );
        assert!(
            navidrome_cacheable(
                &Method::GET,
                "/rest/getSong.view",
                Some("u=alice&p=secret&id=1&f=json&callback=cb"),
                None,
                &policy,
            )
            .is_none()
        );
    }

    #[test]
    fn response_affecting_params_are_in_cache_key() {
        let policy = NavidromeCachePolicy::default();
        let first = navidrome_cacheable(
            &Method::GET,
            "/rest/getAlbum.view",
            Some("u=alice&p=secret&id=1&f=json&v=1.16.1"),
            None,
            &policy,
        )
        .unwrap();
        let second = navidrome_cacheable(
            &Method::GET,
            "/rest/getAlbum.view",
            Some("u=alice&p=secret&id=1&f=json&v=1.16.2"),
            None,
            &policy,
        )
        .unwrap();
        assert_ne!(navidrome_cache_key(&first), navidrome_cache_key(&second));
    }

    #[test]
    fn only_user_state_mutations_force_coarse_invalidation() {
        assert!(navidrome_invalidates_cache("/rest/star.view"));
        assert!(navidrome_invalidates_cache("/rest/unstar"));
        assert!(navidrome_invalidates_cache("/rest/setRating.view"));
        assert!(!navidrome_invalidates_cache("/rest/scrobble"));
        assert!(!navidrome_invalidates_cache("/rest/updatePlaylist.view"));
        assert!(!navidrome_invalidates_cache("/rest/getAlbum.view"));
    }
}
