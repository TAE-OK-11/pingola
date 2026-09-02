use cloudflare_pingora::http::ResponseHeader;
use http::header::{
    CACHE_CONTROL, CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, PRAGMA,
};
use http::header::{HeaderName, HeaderValue};

pub const EXPIRES_HEADER: HeaderName = HeaderName::from_static("expires");
pub const ETAG: HeaderName = HeaderName::from_static("etag");
pub const LAST_MODIFIED: HeaderName = HeaderName::from_static("last-modified");
pub const NO_STORE: HeaderValue = HeaderValue::from_static("no-store");

/// AdGuard DoH responses must not carry UI cache metadata downstream.
pub fn strip_doh_caching_headers(response: &mut ResponseHeader) {
    response.remove_header(&CACHE_CONTROL);
    response.remove_header(&EXPIRES_HEADER);
    response.remove_header(&PRAGMA);
    response.remove_header(&ETAG);
    response.remove_header(&LAST_MODIFIED);
    response.insert_typed_header(CACHE_CONTROL, NO_STORE.clone());
}

pub fn response_status_has_no_body(status: u16) -> bool {
    (100..200).contains(&status) || status == 204 || status == 205 || status == 304
}

pub fn response_status_is_interim(status: u16) -> bool {
    (100..200).contains(&status) && status != 101
}

pub fn response_allows_compression(response: &ResponseHeader) -> bool {
    if response_status_has_no_body(response.status.as_u16())
        || response.status.as_u16() == 206
        || response.headers.contains_key(CONTENT_RANGE)
        || response.headers.contains_key(CONTENT_ENCODING)
    {
        return false;
    }
    if response.headers.get_all(CACHE_CONTROL).iter().any(|value| {
        value.to_str().is_ok_and(|value| {
            value.split(',').any(|directive| {
                directive
                    .trim()
                    .split_once('=')
                    .map_or(directive.trim(), |(name, _)| name.trim())
                    .eq_ignore_ascii_case("no-transform")
            })
        })
    }) {
        return false;
    }
    if let Some(length) = response.headers.get(CONTENT_LENGTH)
        && length
            .to_str()
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .is_none_or(|length| length < 1024)
    {
        return false;
    }

    response
        .headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(compressible_proxy_content_type)
}

fn compressible_proxy_content_type(value: &str) -> bool {
    let essence = value.split(';').next().unwrap_or_default().trim();
    if essence
        .get(..5)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("text/"))
    {
        return true;
    }
    if [
        "application/javascript",
        "application/json",
        "application/ld+json",
        "application/manifest+json",
        "application/xhtml+xml",
        "application/xml",
        "application/rss+xml",
        "image/svg+xml",
    ]
    .iter()
    .any(|candidate| essence.eq_ignore_ascii_case(candidate))
    {
        return true;
    }
    essence.rsplit_once('+').is_some_and(|(_, suffix)| {
        suffix.eq_ignore_ascii_case("json") || suffix.eq_ignore_ascii_case("xml")
    })
}
