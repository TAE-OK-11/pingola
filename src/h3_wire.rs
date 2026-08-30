use std::collections::HashMap;

use bytes::Bytes;
use cloudflare_pingora::http::RequestHeader;
use http::header::HOST;
use tokio_quiche::quiche::h3::{self, NameValue};

fn is_pseudo(name: &[u8]) -> bool {
    !name.is_empty() && name[0] == b':'
}

fn is_hop_by_hop(name: &[u8]) -> bool {
    name.eq_ignore_ascii_case(b"connection")
        || name.eq_ignore_ascii_case(b"transfer-encoding")
        || name.eq_ignore_ascii_case(b"upgrade")
        || name.eq_ignore_ascii_case(b"keep-alive")
        || name.eq_ignore_ascii_case(b"proxy-connection")
}

fn skip_regular_header(name: &[u8], value: &[u8]) -> bool {
    name.eq_ignore_ascii_case(b"host")
        || is_hop_by_hop(name)
        || (name.eq_ignore_ascii_case(b"te") && !value.eq_ignore_ascii_case(b"trailers"))
}

fn lowercase_key<'a>(name: &[u8], scratch: &'a mut Vec<u8>) -> &'a [u8] {
    scratch.clear();
    scratch.extend_from_slice(name);
    for byte in scratch.iter_mut() {
        *byte = byte.to_ascii_lowercase();
    }
    scratch
}

/// Capture the downstream HTTP/3 request header block for upstream passthrough.
pub fn capture_request_wire(headers: &[h3::Header]) -> Vec<h3::Header> {
    headers.to_vec()
}

pub fn headers_to_bytes_pairs(headers: Vec<h3::Header>) -> Vec<(Bytes, Bytes)> {
    headers
        .into_iter()
        .map(|header| {
            (
                Bytes::copy_from_slice(header.name()),
                Bytes::copy_from_slice(header.value()),
            )
        })
        .collect()
}

pub fn bytes_pairs_to_headers(pairs: Vec<(Bytes, Bytes)>) -> Vec<h3::Header> {
    pairs
        .into_iter()
        .map(|(name, value)| h3::Header::new(name.as_ref(), value.as_ref()))
        .collect()
}

/// Reconcile captured wire headers with the filtered upstream `RequestHeader`.
///
/// Pseudo-headers are rebuilt from `req`. Regular headers are taken from `req`
/// while reusing unchanged downstream allocations when possible.
pub fn finalize_upstream_wire(wire: &mut Vec<h3::Header>, req: &RequestHeader) {
    let authority = req
        .headers
        .get(HOST)
        .and_then(|value| value.to_str().ok())
        .or_else(|| req.uri.authority().map(|value| value.as_str()))
        .unwrap_or("");
    let path = req.uri.path_and_query().map_or("/", |value| value.as_str());

    let mut reusable = HashMap::with_capacity(wire.len());
    let mut scratch = Vec::with_capacity(32);
    for header in wire.drain(..) {
        let name = header.name();
        if is_pseudo(name) || skip_regular_header(name, header.value()) {
            continue;
        }
        reusable.insert(lowercase_key(name, &mut scratch).to_vec(), header);
    }

    wire.clear();
    wire.reserve(req.headers.len().saturating_add(4));
    wire.push(h3::Header::new(
        b":method",
        req.method.as_str().as_bytes(),
    ));
    wire.push(h3::Header::new(b":scheme", b"https"));
    wire.push(h3::Header::new(b":authority", authority.as_bytes()));
    wire.push(h3::Header::new(b":path", path.as_bytes()));

    for (name, value) in &req.headers {
        if skip_regular_header(name.as_str().as_bytes(), value.as_bytes()) {
            continue;
        }
        let key = lowercase_key(name.as_str().as_bytes(), &mut scratch).to_vec();
        if let Some(existing) = reusable.remove(&key)
            && existing.value() == value.as_bytes()
        {
            wire.push(existing);
        } else {
            wire.push(h3::Header::new(
                name.as_str().as_bytes(),
                value.as_bytes(),
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use cloudflare_pingora::http::RequestHeader;
    use http::header::{ACCEPT_ENCODING, HOST, USER_AGENT};
    use http::{Method, Version};

    use super::*;

    fn sample_wire() -> Vec<h3::Header> {
        vec![
            h3::Header::new(b":method", b"GET"),
            h3::Header::new(b":scheme", b"https"),
            h3::Header::new(b":authority", b"music.example"),
            h3::Header::new(b":path", b"/rest/stream?id=1"),
            h3::Header::new(b"user-agent", b"navidrome-client/1.0"),
            h3::Header::new(b"accept-encoding", b"gzip"),
            h3::Header::new(b"forwarded", b"for=1.2.3.4"),
        ]
    }

    #[test]
    fn finalize_rebuilds_pseudos_and_forwarded_headers() {
        let mut wire = sample_wire();
        let mut req = RequestHeader::build(Method::GET, b"/rest/stream?id=1", None).unwrap();
        req.set_version(Version::HTTP_3);
        req.insert_header(HOST, "origin.internal").unwrap();
        req.insert_header(USER_AGENT, "navidrome-client/1.0").unwrap();
        req.insert_header(ACCEPT_ENCODING, "gzip").unwrap();
        req.insert_header("x-forwarded-for", "203.0.113.1").unwrap();
        req.insert_header("x-real-ip", "203.0.113.1").unwrap();

        finalize_upstream_wire(&mut wire, &req);

        assert_eq!(wire[0].name(), b":method");
        assert_eq!(wire[0].value(), b"GET");
        assert_eq!(wire[2].value(), b"origin.internal");
        assert!(wire.iter().any(|header| header.name() == b"user-agent"));
        assert!(wire.iter().any(|header| {
            header.name() == b"x-forwarded-for" && header.value() == b"203.0.113.1"
        }));
        assert!(!wire.iter().any(|header| header.name() == b"forwarded"));
        assert!(!wire.iter().any(|header| header.name() == b"host"));
    }

    #[test]
    fn finalize_reuses_unchanged_regular_header_allocations() {
        let mut wire = sample_wire();
        let user_agent = wire[4].value().to_vec();
        let mut req = RequestHeader::build(Method::GET, b"/rest/stream?id=1", None).unwrap();
        req.set_version(Version::HTTP_3);
        req.insert_header(HOST, "origin.internal").unwrap();
        req.insert_header(USER_AGENT, "navidrome-client/1.0").unwrap();

        finalize_upstream_wire(&mut wire, &req);

        let reused = wire
            .iter()
            .find(|header| header.name() == b"user-agent")
            .expect("user-agent");
        assert_eq!(reused.value(), user_agent.as_slice());
    }

    #[test]
    fn bytes_pair_roundtrip_preserves_headers() {
        let wire = sample_wire();
        let pairs = headers_to_bytes_pairs(wire.clone());
        let restored = bytes_pairs_to_headers(pairs);
        assert_eq!(restored.len(), wire.len());
        for (left, right) in wire.iter().zip(restored.iter()) {
            assert_eq!(left.name(), right.name());
            assert_eq!(left.value(), right.value());
        }
    }
}
