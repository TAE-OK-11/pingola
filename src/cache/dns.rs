//! DNS wire-format helpers for DoH caching.
//!
//! Keys account for normalized qname, qtype, qclass, CD, and EDNS DO state.
//! Cached responses age TTL fields before they are served locally.

use std::time::{Duration, Instant};

use ahash::AHasher;
use bytes::{Bytes, BytesMut};
use std::hash::Hasher;

use crate::cache::core::{CacheKey, CacheNamespace, CachedValue};

const EDNS0_TYPE: u16 = 41;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DnsQueryKey {
    pub qname: String,
    pub qtype: u16,
    pub qclass: u16,
    /// Recursion desired.
    pub rd: bool,
    /// Checking disabled (DNSSEC).
    pub cd: bool,
    /// DNSSEC OK (EDNS DO bit).
    pub do_bit: bool,
}

#[derive(Clone, Copy, Debug)]
pub struct DnsCachePolicy {
    pub enabled: bool,
    pub max_ttl: Duration,
    pub negative_ttl: Duration,
    pub max_response_bytes: usize,
}

impl Default for DnsCachePolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            max_ttl: Duration::from_secs(300),
            negative_ttl: Duration::from_secs(60),
            max_response_bytes: 65535,
        }
    }
}

pub fn parse_doh_query(method: &str, path: &str, body: &[u8]) -> Option<Vec<u8>> {
    if method.eq_ignore_ascii_case("GET") {
        let query = path.split('?').nth(1)?;
        for pair in query.split('&') {
            let (name, value) = pair.split_once('=')?;
            if name == "dns" {
                return decode_base64url(value);
            }
        }
        None
    } else if method.eq_ignore_ascii_case("POST") && !body.is_empty() {
        Some(body.to_vec())
    } else {
        None
    }
}

pub fn dns_query_key(wire: &[u8]) -> Option<DnsQueryKey> {
    if wire.len() < 12 {
        return None;
    }
    let flags = u16::from_be_bytes([wire[2], wire[3]]);
    if flags & 0x8000 != 0 {
        return None;
    }
    let qdcount = u16::from_be_bytes([wire[4], wire[5]]);
    if qdcount != 1 {
        return None;
    }
    let rd = flags & 0x0100 != 0;
    let cd = flags & 0x0010 != 0;
    let mut offset = 12usize;
    let qname = read_name(wire, &mut offset)?;
    if wire.len() < offset + 4 {
        return None;
    }
    let qtype = u16::from_be_bytes([wire[offset], wire[offset + 1]]);
    let qclass = u16::from_be_bytes([wire[offset + 2], wire[offset + 3]]);
    offset += 4;
    let do_bit = edns_do_bit(wire, offset)?;
    Some(DnsQueryKey {
        qname,
        qtype,
        qclass,
        rd,
        cd,
        do_bit,
    })
}

pub fn cache_key_for_query(query: &DnsQueryKey) -> CacheKey {
    let mut hasher = AHasher::default();
    hasher.write(query.qname.as_bytes());
    hasher.write_u16(query.qtype);
    hasher.write_u16(query.qclass);
    hasher.write_u8(query.rd as u8);
    hasher.write_u8(query.cd as u8);
    hasher.write_u8(query.do_bit as u8);
    CacheKey::new(CacheNamespace::Dns, hasher.finish())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DnsCacheDecision {
    Cacheable { ttl: Duration },
    NotCacheable,
}

pub fn dns_cacheable(response: &[u8], policy: &DnsCachePolicy) -> DnsCacheDecision {
    if !policy.enabled || response.len() < 12 || response.len() > policy.max_response_bytes {
        return DnsCacheDecision::NotCacheable;
    }
    let flags = u16::from_be_bytes([response[2], response[3]]);
    if flags & 0x0200 != 0 {
        return DnsCacheDecision::NotCacheable;
    }
    let rcode = flags & 0x000F;
    match rcode {
        0 => {}
        3 => {
            let ttl = negative_ttl(response).unwrap_or(policy.negative_ttl);
            return DnsCacheDecision::Cacheable {
                ttl: ttl.min(policy.max_ttl),
            };
        }
        1 | 2 | 5 => return DnsCacheDecision::NotCacheable,
        _ => return DnsCacheDecision::NotCacheable,
    }
    let Some(min_ttl) = min_rr_ttl(response) else {
        return DnsCacheDecision::NotCacheable;
    };
    if min_ttl.is_zero() {
        return DnsCacheDecision::NotCacheable;
    }
    DnsCacheDecision::Cacheable {
        ttl: min_ttl.min(policy.max_ttl),
    }
}

pub fn build_cached_dns(response: &[u8], ttl: Duration, now: Instant) -> CachedValue {
    CachedValue {
        body: Bytes::copy_from_slice(response),
        stored_at: now,
        fresh_until: now + ttl,
    }
}

pub fn age_dns_response(response: &Bytes, stored_at: Instant, now: Instant) -> Option<Bytes> {
    let elapsed = now.saturating_duration_since(stored_at);
    if elapsed.is_zero() {
        return Some(response.clone());
    }
    let elapsed_secs = elapsed.as_secs().min(u32::MAX as u64) as u32;
    let mut out = response.to_vec();
    if out.len() < 12 {
        return None;
    }
    let qdcount = u16::from_be_bytes([out[4], out[5]]) as usize;
    let mut offset = 12usize;
    for _ in 0..qdcount {
        skip_name(&out, &mut offset)?;
        offset = offset.checked_add(4)?;
    }
    let ancount = u16::from_be_bytes([out[6], out[7]]) as usize;
    let nscount = u16::from_be_bytes([out[8], out[9]]) as usize;
    let arcount = u16::from_be_bytes([out[10], out[11]]) as usize;
    let mut min_remaining = u32::MAX;
    for _ in 0..(ancount + nscount + arcount) {
        skip_name(&out, &mut offset)?;
        if out.len() < offset + 10 {
            return None;
        }
        let ttl_offset = offset + 4;
        let ttl = u32::from_be_bytes([
            out[ttl_offset],
            out[ttl_offset + 1],
            out[ttl_offset + 2],
            out[ttl_offset + 3],
        ]);
        let remaining = ttl.saturating_sub(elapsed_secs);
        min_remaining = min_remaining.min(remaining);
        out[ttl_offset..ttl_offset + 4].copy_from_slice(&remaining.to_be_bytes());
        let rdlength = u16::from_be_bytes([out[offset + 8], out[offset + 9]]) as usize;
        offset = offset.checked_add(10 + rdlength)?;
    }
    if min_remaining == 0 {
        None
    } else {
        Some(Bytes::from(out))
    }
}

fn negative_ttl(response: &[u8]) -> Option<Duration> {
    let mut offset = skip_questions(response, 12)?;
    let nscount = u16::from_be_bytes([response[8], response[9]]) as usize;
    let mut min = None;
    for _ in 0..nscount {
        skip_name(response, &mut offset)?;
        if response.len() < offset + 10 {
            return None;
        }
        let rtype = u16::from_be_bytes([response[offset], response[offset + 1]]);
        if rtype == 6 {
            let ttl = u32::from_be_bytes([
                response[offset + 4],
                response[offset + 5],
                response[offset + 6],
                response[offset + 7],
            ]);
            let rdlength =
                u16::from_be_bytes([response[offset + 8], response[offset + 9]]) as usize;
            if response.len() >= offset + 10 + rdlength && rdlength >= 7 {
                let minimum = u32::from_be_bytes([
                    response[offset + 10 + rdlength - 4],
                    response[offset + 10 + rdlength - 3],
                    response[offset + 10 + rdlength - 2],
                    response[offset + 10 + rdlength - 1],
                ]);
                let candidate = minimum.min(ttl);
                min = Some(min.map_or(candidate, |current: u32| current.min(candidate)));
            }
        }
        let rdlength = u16::from_be_bytes([response[offset + 8], response[offset + 9]]) as usize;
        offset = offset.checked_add(10 + rdlength)?;
    }
    min.map(|seconds| Duration::from_secs(seconds.max(1) as u64))
}

fn min_rr_ttl(response: &[u8]) -> Option<Duration> {
    let mut offset = skip_questions(response, 12)?;
    let ancount = u16::from_be_bytes([response[6], response[7]]) as usize;
    let nscount = u16::from_be_bytes([response[8], response[9]]) as usize;
    let arcount = u16::from_be_bytes([response[10], response[11]]) as usize;
    let mut min = None;
    for _ in 0..(ancount + nscount + arcount) {
        skip_name(response, &mut offset)?;
        if response.len() < offset + 10 {
            return None;
        }
        let ttl = u32::from_be_bytes([
            response[offset + 4],
            response[offset + 5],
            response[offset + 6],
            response[offset + 7],
        ]);
        min = Some(min.map_or(ttl, |current: u32| current.min(ttl)));
        let rdlength = u16::from_be_bytes([response[offset + 8], response[offset + 9]]) as usize;
        offset = offset.checked_add(10 + rdlength)?;
    }
    min.filter(|ttl| *ttl > 0)
        .map(|seconds| Duration::from_secs(seconds as u64))
}

fn skip_questions(response: &[u8], mut offset: usize) -> Option<usize> {
    let qdcount = u16::from_be_bytes([response[4], response[5]]) as usize;
    for _ in 0..qdcount {
        skip_name(response, &mut offset)?;
        offset = offset.checked_add(4)?;
    }
    Some(offset)
}

fn edns_do_bit(response: &[u8], mut offset: usize) -> Option<bool> {
    let ancount = u16::from_be_bytes([response[6], response[7]]) as usize;
    let nscount = u16::from_be_bytes([response[8], response[9]]) as usize;
    offset = skip_rrs(response, offset, ancount + nscount)?;
    let arcount = u16::from_be_bytes([response[10], response[11]]) as usize;
    for _ in 0..arcount {
        skip_name(response, &mut offset)?;
        if response.len() < offset + 10 {
            return None;
        }
        let rtype = u16::from_be_bytes([response[offset], response[offset + 1]]);
        let ttl = u32::from_be_bytes([
            response[offset + 4],
            response[offset + 5],
            response[offset + 6],
            response[offset + 7],
        ]);
        let rdlength = u16::from_be_bytes([response[offset + 8], response[offset + 9]]) as usize;
        if rtype == EDNS0_TYPE {
            let do_bit = (ttl >> 15) & 1 == 1;
            return Some(do_bit);
        }
        offset = offset.checked_add(10 + rdlength)?;
    }
    Some(false)
}

fn skip_rrs(response: &[u8], mut offset: usize, count: usize) -> Option<usize> {
    for _ in 0..count {
        skip_name(response, &mut offset)?;
        if response.len() < offset + 10 {
            return None;
        }
        let rdlength = u16::from_be_bytes([response[offset + 8], response[offset + 9]]) as usize;
        offset = offset.checked_add(10 + rdlength)?;
    }
    Some(offset)
}

fn read_name(response: &[u8], offset: &mut usize) -> Option<String> {
    let mut labels = Vec::new();
    read_name_labels(response, offset, &mut labels, 0)
}

fn read_name_labels(
    response: &[u8],
    offset: &mut usize,
    labels: &mut Vec<String>,
    depth: u8,
) -> Option<String> {
    if depth > 8 {
        return None;
    }
    loop {
        if *offset >= response.len() {
            return None;
        }
        let len = response[*offset];
        if len == 0 {
            *offset += 1;
            break;
        }
        if len & 0xC0 == 0xC0 {
            if *offset + 1 >= response.len() {
                return None;
            }
            let mut pointer =
                u16::from_be_bytes([response[*offset] & 0x3F, response[*offset + 1]]) as usize;
            *offset += 2;
            let mut pointer_labels = labels.clone();
            read_name_labels(response, &mut pointer, &mut pointer_labels, depth + 1)?;
            labels.clear();
            labels.extend(pointer_labels);
            break;
        }
        *offset += 1;
        let end = (*offset).checked_add(len as usize)?;
        if end > response.len() {
            return None;
        }
        let label = std::str::from_utf8(&response[*offset..end]).ok()?;
        labels.push(label.to_ascii_lowercase());
        *offset = end;
    }
    Some(labels.join("."))
}

fn skip_name(response: &[u8], offset: &mut usize) -> Option<()> {
    loop {
        if *offset >= response.len() {
            return None;
        }
        let len = response[*offset];
        if len == 0 {
            *offset += 1;
            return Some(());
        }
        if len & 0xC0 == 0xC0 {
            *offset += 2;
            return Some(());
        }
        *offset += 1;
        *offset = (*offset).checked_add(len as usize)?;
    }
}

fn decode_base64url(input: &str) -> Option<Vec<u8>> {
    let mut output = Vec::with_capacity(input.len() * 3 / 4 + 4);
    let mut buffer = 0u32;
    let mut bits = 0u32;
    for ch in input.bytes() {
        let value = match ch {
            b'A'..=b'Z' => ch - b'A',
            b'a'..=b'z' => ch - b'a' + 26,
            b'0'..=b'9' => ch - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            b'=' => continue,
            _ => return None,
        };
        buffer = (buffer << 6) | value as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            output.push((buffer >> bits) as u8);
            buffer &= (1 << bits) - 1;
        }
    }
    Some(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode_name(name: &str) -> Vec<u8> {
        let mut out = Vec::new();
        let normalized = name.trim_end_matches('.');
        if normalized.is_empty() {
            out.push(0);
            return out;
        }
        for label in normalized.split('.') {
            out.push(label.len() as u8);
            out.extend_from_slice(label.as_bytes());
        }
        out.push(0);
        out
    }

    fn build_query(name: &str, qtype: u16, qclass: u16, rd: bool, cd: bool) -> Vec<u8> {
        let mut msg = vec![
            0xAA, 0xBB, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ];
        let mut flags = 0u16;
        if rd {
            flags |= 0x0100;
        }
        if cd {
            flags |= 0x0010;
        }
        msg[2..4].copy_from_slice(&flags.to_be_bytes());
        msg.extend(encode_name(name));
        msg.extend_from_slice(&qtype.to_be_bytes());
        msg.extend_from_slice(&qclass.to_be_bytes());
        msg
    }

    fn build_response(name: &str, qtype: u16, ttl: u32, rdata: &[u8], rcode: u16) -> Vec<u8> {
        let mut msg = build_query(name, qtype, 1, true, false);
        msg[2] = 0x80 | msg[2];
        msg[3] = (msg[3] & 0xF0) as u8 | (rcode as u8 & 0x0F);
        msg[6..8].copy_from_slice(&1u16.to_be_bytes());
        msg.extend(encode_name(name));
        msg.extend_from_slice(&qtype.to_be_bytes());
        msg.extend_from_slice(&1u16.to_be_bytes());
        msg.extend_from_slice(&ttl.to_be_bytes());
        msg.extend_from_slice(&(rdata.len() as u16).to_be_bytes());
        msg.extend_from_slice(rdata);
        msg
    }

    #[test]
    fn parses_query_and_isolates_qtypes() {
        let a = build_query("Example.COM.", 1, 1, true, false);
        let aaaa = build_query("example.com", 28, 1, true, false);
        let key_a = dns_query_key(&a).unwrap();
        let key_aaaa = dns_query_key(&aaaa).unwrap();
        assert_eq!(key_a.qname, "example.com");
        assert_eq!(key_a.qtype, 1);
        assert_eq!(key_aaaa.qtype, 28);
        assert_ne!(cache_key_for_query(&key_a), cache_key_for_query(&key_aaaa));
    }

    #[test]
    fn cd_bit_isolated_in_cache_key() {
        let plain = dns_query_key(&build_query("example.com", 1, 1, true, false)).unwrap();
        let cd = dns_query_key(&build_query("example.com", 1, 1, true, true)).unwrap();
        assert_ne!(cache_key_for_query(&plain), cache_key_for_query(&cd));
    }

    #[test]
    fn ages_ttl_on_serve() {
        let response = Bytes::from(build_response("example.com", 1, 120, &[192, 0, 2, 1], 0));
        let stored_at = Instant::now() - Duration::from_secs(30);
        let aged = age_dns_response(&response, stored_at, Instant::now()).unwrap();
        let mut offset = 12;
        skip_name(&aged, &mut offset).unwrap();
        offset += 4;
        skip_name(&aged, &mut offset).unwrap();
        let ttl = u32::from_be_bytes([
            aged[offset + 4],
            aged[offset + 5],
            aged[offset + 6],
            aged[offset + 7],
        ]);
        assert_eq!(ttl, 90);
    }

    #[test]
    fn negative_cache_uses_soa_minimum() {
        let mut msg = build_query("missing.example", 1, 1, true, false);
        msg[2] = 0x80;
        msg[3] = 3;
        msg[8..10].copy_from_slice(&1u16.to_be_bytes());
        msg.extend(encode_name("missing.example"));
        msg.extend_from_slice(&6u16.to_be_bytes());
        msg.extend_from_slice(&1u16.to_be_bytes());
        msg.extend_from_slice(&300u32.to_be_bytes());
        let mut rdata = encode_name("ns.example");
        rdata.extend_from_slice(b"host.example.");
        rdata.extend_from_slice(&1u32.to_be_bytes());
        rdata.extend_from_slice(&3600u32.to_be_bytes());
        rdata.extend_from_slice(&86400u32.to_be_bytes());
        rdata.extend_from_slice(&120u32.to_be_bytes());
        rdata.extend_from_slice(&60u32.to_be_bytes());
        msg.extend_from_slice(&(rdata.len() as u16).to_be_bytes());
        msg.extend_from_slice(&rdata);
        let decision = dns_cacheable(&msg, &DnsCachePolicy::default());
        assert!(matches!(decision, DnsCacheDecision::Cacheable { .. }));
    }

    #[test]
    fn servfail_is_not_cacheable() {
        let response = build_response("example.com", 1, 60, &[1, 2, 3, 4], 2);
        assert_eq!(
            dns_cacheable(&response, &DnsCachePolicy::default()),
            DnsCacheDecision::NotCacheable
        );
    }
}
