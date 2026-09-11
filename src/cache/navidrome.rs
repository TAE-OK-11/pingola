//! Conservative Navidrome / OpenSubsonic metadata cache helpers.

use std::time::{Duration, Instant};

use ahash::AHasher;
use bytes::Bytes;
use http::Method;
use std::hash::Hasher;

use crate::cache::core::{CacheKey, CacheNamespace, CachedValue};

const CACHEABLE_SUFFIXES: &[&str] = &[
    "getAlbum",
    "getArtist",
    "getSong",
    "getGenres",
    "getMusicFolders",
    "getAlbumInfo",
    "getArtistInfo",
    "getSongInfo",
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
    let user_scope = user_scope(&params);
    if user_scope.is_empty() {
        return None;
    }
    let stable_params = stable_query_params(&endpoint, &params);
    Some(NavidromeCacheable {
        endpoint,
        user_scope,
        stable_params,
    })
}

pub fn navidrome_cache_key(entry: &NavidromeCacheable) -> CacheKey {
    let mut hasher = AHasher::default();
    hasher.write(entry.endpoint.as_bytes());
    hasher.write(entry.user_scope.as_bytes());
    for (key, value) in &entry.stable_params {
        hasher.write(key.as_bytes());
        hasher.write(value.as_bytes());
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
    if response_body.len() > policy.max_response_bytes {
        return None;
    }
    if looks_like_xml_error(response_body) {
        return None;
    }
    let ttl = cache_control_max_age
        .map(Duration::from_secs)
        .unwrap_or(policy.default_ttl);
    Some(ttl.min(policy.max_ttl))
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
    pairs.sort_by(|left, right| left.0.cmp(&right.0));
    pairs
}

fn user_scope(params: &[(String, String)]) -> String {
    if let Some(api_key) = params.iter().find(|(key, _)| key == "apiKey") {
        let mut hasher = AHasher::default();
        hasher.write(api_key.1.as_bytes());
        return format!("apiKey:{:016x}", hasher.finish());
    }
    params
        .iter()
        .find(|(key, _)| key == "u")
        .map(|(_, value)| format!("u:{value}"))
        .unwrap_or_default()
}

fn stable_query_params(endpoint: &str, params: &[(String, String)]) -> Vec<(String, String)> {
    let ignored = [
        "u", "t", "s", "p", "v", "c", "f", "apiKey", "jwt", "callback",
    ];
    params
        .iter()
        .filter(|(key, _)| {
            !ignored.contains(&key.as_str()) && !key.ends_with("Token") && !key.ends_with("Salt")
        })
        .filter(|(key, _)| endpoint_specific_param(endpoint, key))
        .cloned()
        .collect()
}

fn endpoint_specific_param(endpoint: &str, key: &str) -> bool {
    match endpoint.to_ascii_lowercase().as_str() {
        "getalbum" | "getalbuminfo" => key == "id",
        "getartist" | "getartistinfo" => key == "id",
        "getsong" | "getsonginfo" => key == "id",
        "getgenres" | "getmusicfolders" => true,
        _ => key == "id",
    }
}

fn looks_like_xml_error(body: &[u8]) -> bool {
    let prefix = &body[..body.len().min(256)];
    let text = String::from_utf8_lossy(prefix);
    text.contains("<error ") || text.contains("\"status\":\"failed\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_allows_conservative_endpoints() {
        let policy = NavidromeCachePolicy::default();
        assert!(
            navidrome_cacheable(
                &Method::GET,
                "/rest/getAlbum.view",
                Some("u=alice&id=1"),
                &policy
            )
            .is_some()
        );
        assert!(
            navidrome_cacheable(
                &Method::GET,
                "/rest/scrobble.view",
                Some("u=alice&id=1"),
                &policy
            )
            .is_none()
        );
    }

    #[test]
    fn user_scope_is_isolated() {
        let policy = NavidromeCachePolicy::default();
        let alice = navidrome_cacheable(
            &Method::GET,
            "/rest/getAlbum.view",
            Some("u=alice&id=1"),
            &policy,
        )
        .unwrap();
        let bob = navidrome_cacheable(
            &Method::GET,
            "/rest/getAlbum.view",
            Some("u=bob&id=1"),
            &policy,
        )
        .unwrap();
        assert_ne!(navidrome_cache_key(&alice), navidrome_cache_key(&bob));
    }

    #[test]
    fn auth_params_do_not_affect_cache_key() {
        let policy = NavidromeCachePolicy::default();
        let first = navidrome_cacheable(
            &Method::GET,
            "/rest/getSong.view",
            Some("u=alice&id=9&t=one&s=salt"),
            &policy,
        )
        .unwrap();
        let second = navidrome_cacheable(
            &Method::GET,
            "/rest/getSong.view",
            Some("u=alice&id=9&t=two&s=other"),
            &policy,
        )
        .unwrap();
        assert_eq!(navidrome_cache_key(&first), navidrome_cache_key(&second));
    }
}
