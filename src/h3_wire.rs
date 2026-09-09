use std::collections::HashMap;

use bytes::Bytes;
use cloudflare_pingora::http::RequestHeader;
use http::header::HOST;
use tokio_quiche::quiche::h3;

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

#[cfg(test)]
fn lowercase_key<'a>(name: &[u8], scratch: &'a mut Vec<u8>) -> &'a [u8] {
    scratch.clear();
    scratch.extend_from_slice(name);
    for byte in scratch.iter_mut() {
        *byte = byte.to_ascii_lowercase();
    }
    scratch
}

/// Convert the downstream HTTP/3 request header block into byte pairs once.
///
/// The QUIC stack hands us an owned `h3::Header` (`(Vec<u8>, Vec<u8>)`). Steal
/// those buffers into `Bytes` so the proxy hot path does not memcpy every name
/// and value before upstream open.
pub fn headers_to_bytes_pairs(headers: Vec<h3::Header>) -> Vec<(Bytes, Bytes)> {
    headers.into_iter().map(header_into_bytes_pair).collect()
}

fn header_into_bytes_pair(header: h3::Header) -> (Bytes, Bytes) {
    // quiche::h3::Header is a private tuple struct of two Vec<u8> fields. Its
    // layout matches `(Vec<u8>, Vec<u8>)`; a regression test locks this in.
    // Safety: Header is `#[derive(...)] struct Header(Vec<u8>, Vec<u8>)` with
    // no #[repr] override. Transmuting into the equivalent tuple takes ownership
    // of the same allocations without copying.
    let (name, value): (Vec<u8>, Vec<u8>) =
        unsafe { std::mem::transmute::<h3::Header, (Vec<u8>, Vec<u8>)>(header) };
    (Bytes::from(name), Bytes::from(value))
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
#[cfg(test)]
pub fn finalize_upstream_wire(wire: &mut Vec<h3::Header>, req: &RequestHeader) {
    use tokio_quiche::quiche::h3::NameValue;

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
    wire.push(h3::Header::new(b":method", req.method.as_str().as_bytes()));
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
            wire.push(h3::Header::new(name.as_str().as_bytes(), value.as_bytes()));
        }
    }
}

/// Same as [`finalize_upstream_wire`] but operates on trait-bound byte pairs
/// without converting through `h3::Header`.
pub fn finalize_upstream_wire_pairs(wire: &mut Vec<(Bytes, Bytes)>, req: &RequestHeader) {
    let authority = req
        .headers
        .get(HOST)
        .and_then(|value| value.to_str().ok())
        .or_else(|| req.uri.authority().map(|value| value.as_str()))
        .unwrap_or("");
    let path = req.uri.path_and_query().map_or("/", |value| value.as_str());

    let mut reusable = HashMap::with_capacity(wire.len());
    for (name, value) in wire.drain(..) {
        if is_pseudo(&name) || skip_regular_header(&name, &value) {
            continue;
        }
        // HTTP/3 names are already lowercase. Share their Bytes allocation
        // instead of allocating a lowercase Vec for every insert and lookup.
        // Keep normalization for callers supplying mixed-case byte pairs.
        let key = if name.iter().any(u8::is_ascii_uppercase) {
            Bytes::from(name.to_ascii_lowercase())
        } else {
            name.clone()
        };
        reusable.insert(key, (name, value));
    }

    wire.clear();
    wire.reserve(req.headers.len().saturating_add(4));
    wire.push((
        Bytes::from_static(b":method"),
        Bytes::copy_from_slice(req.method.as_str().as_bytes()),
    ));
    wire.push((Bytes::from_static(b":scheme"), Bytes::from_static(b"https")));
    wire.push((
        Bytes::from_static(b":authority"),
        Bytes::copy_from_slice(authority.as_bytes()),
    ));
    wire.push((
        Bytes::from_static(b":path"),
        Bytes::copy_from_slice(path.as_bytes()),
    ));

    for (name, value) in &req.headers {
        if skip_regular_header(name.as_str().as_bytes(), value.as_bytes()) {
            continue;
        }
        if let Some((existing_name, existing_value)) = reusable.remove(name.as_str().as_bytes())
            && existing_value.as_ref() == value.as_bytes()
        {
            wire.push((existing_name, existing_value));
        } else {
            wire.push((
                Bytes::copy_from_slice(name.as_str().as_bytes()),
                Bytes::copy_from_slice(value.as_bytes()),
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use cloudflare_pingora::http::RequestHeader;
    use http::header::{ACCEPT_ENCODING, HOST, USER_AGENT};
    use http::{Method, Version};
    use tokio_quiche::quiche::h3::NameValue;

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
    fn headers_to_bytes_pairs_steals_header_buffers_without_copying() {
        let wire = vec![
            h3::Header::new(b":method", b"GET"),
            h3::Header::new(b"x-test", b"value"),
        ];
        let name_ptr = wire[1].name().as_ptr();
        let value_ptr = wire[1].value().as_ptr();

        let pairs = headers_to_bytes_pairs(wire);
        assert_eq!(pairs[1].0.as_ref(), b"x-test");
        assert_eq!(pairs[1].1.as_ref(), b"value");
        assert_eq!(pairs[1].0.as_ptr(), name_ptr);
        assert_eq!(pairs[1].1.as_ptr(), value_ptr);
    }

    #[test]
    fn finalize_rebuilds_pseudos_and_forwarded_headers() {
        let mut wire = sample_wire();
        let mut req = RequestHeader::build(Method::GET, b"/rest/stream?id=1", None).unwrap();
        req.set_version(Version::HTTP_3);
        req.insert_header(HOST, "origin.internal").unwrap();
        req.insert_header(USER_AGENT, "navidrome-client/1.0")
            .unwrap();
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
        req.insert_header(USER_AGENT, "navidrome-client/1.0")
            .unwrap();

        finalize_upstream_wire(&mut wire, &req);

        let reused = wire
            .iter()
            .find(|header| header.name() == b"user-agent")
            .expect("user-agent");
        assert_eq!(reused.value(), user_agent.as_slice());
    }

    #[test]
    fn finalize_pairs_matches_header_finalize() {
        let mut wire_headers = sample_wire();
        let mut wire_pairs = headers_to_bytes_pairs(sample_wire());
        let mut req = RequestHeader::build(Method::GET, b"/rest/stream?id=1", None).unwrap();
        req.set_version(Version::HTTP_3);
        req.insert_header(HOST, "origin.internal").unwrap();
        req.insert_header(USER_AGENT, "navidrome-client/1.0")
            .unwrap();
        req.insert_header("x-forwarded-for", "203.0.113.1").unwrap();

        finalize_upstream_wire(&mut wire_headers, &req);
        finalize_upstream_wire_pairs(&mut wire_pairs, &req);

        assert_eq!(wire_pairs.len(), wire_headers.len());
        for ((pair_name, pair_value), header) in wire_pairs.iter().zip(wire_headers.iter()) {
            assert_eq!(pair_name.as_ref(), header.name());
            assert_eq!(pair_value.as_ref(), header.value());
        }
    }

    #[test]
    fn finalize_pairs_preserves_duplicates_and_reuses_lowercase_buffers() {
        let name = Bytes::from(Vec::from(&b"x-value"[..]));
        let value = Bytes::from(Vec::from(&b"second"[..]));
        let name_ptr = name.as_ptr();
        let value_ptr = value.as_ptr();
        let mut wire = vec![(name, value)];
        let mut req = RequestHeader::build(Method::GET, b"/", None).unwrap();
        req.append_header("x-value", "second").unwrap();
        req.append_header("x-value", "first").unwrap();
        finalize_upstream_wire_pairs(&mut wire, &req);
        let values: Vec<_> = wire
            .iter()
            .filter(|(name, _)| name.as_ref() == b"x-value")
            .collect();
        assert_eq!(values.len(), 2);
        assert_eq!(values[0].0.as_ptr(), name_ptr);
        assert_eq!(values[0].1.as_ptr(), value_ptr);
        assert_eq!(values[1].1.as_ref(), b"first");
    }

    #[test]
    fn finalize_pairs_normalizes_lookup_and_discards_replaced_values() {
        let mut wire = vec![
            (Bytes::from_static(b"X-Value"), Bytes::from_static(b"old")),
            (Bytes::from_static(b"forwarded"), Bytes::from_static(b"spoof")),
        ];
        let mut req = RequestHeader::build(Method::GET, b"/", None).unwrap();
        req.insert_header("x-value", "new").unwrap();
        finalize_upstream_wire_pairs(&mut wire, &req);
        assert_eq!(wire.len(), 5);
        assert_eq!(wire[4].0.as_ref(), b"x-value");
        assert_eq!(wire[4].1.as_ref(), b"new");
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
