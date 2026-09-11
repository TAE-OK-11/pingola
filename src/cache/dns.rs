//! DNS wire-format helpers for DoH caching.
//!
//! Keys account for normalized qname, qtype, qclass, DNSSEC state and the
//! complete EDNS OPT state that can affect an answer. Cached responses age
//! real DNS RR TTL fields before they are served locally; EDNS OPT pseudo-TTL
//! fields are never modified.

use std::hash::Hasher;
use std::time::{Duration, Instant};

use ahash::AHasher;
use bytes::Bytes;

use crate::cache::core::{CacheKey, CacheNamespace, CachedValue};

const EDNS0_TYPE: u16 = 41;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DnsQueryKey {
    pub qname: String,
    pub qtype: u16,
    pub qclass: u16,
    pub rd: bool,
    pub cd: bool,
    pub do_bit: bool,
    /// Hash of the complete OPT pseudo-RR state. This keeps ECS, cookies,
    /// payload size, version and future EDNS options from aliasing answers.
    pub edns_fingerprint: u64,
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
    if u16::from_be_bytes([wire[4], wire[5]]) != 1 {
        return None;
    }

    let mut offset = 12usize;
    let qname = read_name(wire, &mut offset)?;
    if wire.len() < offset + 4 {
        return None;
    }
    let qtype = u16::from_be_bytes([wire[offset], wire[offset + 1]]);
    let qclass = u16::from_be_bytes([wire[offset + 2], wire[offset + 3]]);
    offset += 4;
    let (do_bit, edns_fingerprint) = edns_state(wire, offset)?;

    Some(DnsQueryKey {
        qname,
        qtype,
        qclass,
        rd: flags & 0x0100 != 0,
        cd: flags & 0x0010 != 0,
        do_bit,
        edns_fingerprint,
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
    hasher.write_u64(query.edns_fingerprint);
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
    match flags & 0x000F {
        0 => {}
        3 => {
            let ttl = negative_ttl(response).unwrap_or(policy.negative_ttl);
            return DnsCacheDecision::Cacheable {
                ttl: ttl.min(policy.max_ttl),
            };
        }
        // FORMERR / SERVFAIL / REFUSED and every other error stay uncached.
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

/// Backward-compatible materializer used by older call sites that do not have
/// the current query wire available. DoH recommends a zero DNS transaction ID,
/// so use zero rather than leaking the transaction ID from the request that
/// originally populated the cache.
#[allow(dead_code)]
pub fn age_dns_response(response: &Bytes, stored_at: Instant, now: Instant) -> Option<Bytes> {
    age_dns_response_for_query(response, stored_at, now, 0)
}

/// Materialize a cached response for the current query.
///
/// The DNS transaction ID is per query and intentionally absent from the cache
/// key. TYPE 41 OPT's four-byte pseudo-TTL is extended RCODE/version/flags and
/// must never be aged like a normal DNS RR TTL.
pub fn age_dns_response_for_query(
    response: &Bytes,
    stored_at: Instant,
    now: Instant,
    query_id: u16,
) -> Option<Bytes> {
    if response.len() < 12 {
        return None;
    }

    let elapsed_secs = now
        .saturating_duration_since(stored_at)
        .as_secs()
        .min(u32::MAX as u64) as u32;
    let mut out = response.to_vec();
    out[0..2].copy_from_slice(&query_id.to_be_bytes());

    let mut offset = skip_questions(&out, 12)?;
    let ancount = u16::from_be_bytes([out[6], out[7]]) as usize;
    let nscount = u16::from_be_bytes([out[8], out[9]]) as usize;
    let arcount = u16::from_be_bytes([out[10], out[11]]) as usize;
    let mut saw_real_ttl = false;

    for _ in 0..(ancount + nscount + arcount) {
        skip_name(&out, &mut offset)?;
        if out.len() < offset + 10 {
            return None;
        }
        let rtype = u16::from_be_bytes([out[offset], out[offset + 1]]);
        let rdlength = u16::from_be_bytes([out[offset + 8], out[offset + 9]]) as usize;
        let next = offset.checked_add(10 + rdlength)?;
        if next > out.len() {
            return None;
        }

        if rtype != EDNS0_TYPE {
            saw_real_ttl = true;
            let ttl_offset = offset + 4;
            let ttl = u32::from_be_bytes([
                out[ttl_offset],
                out[ttl_offset + 1],
                out[ttl_offset + 2],
                out[ttl_offset + 3],
            ]);
            let remaining = ttl.saturating_sub(elapsed_secs);
            if remaining == 0 {
                return None;
            }
            out[ttl_offset..ttl_offset + 4].copy_from_slice(&remaining.to_be_bytes());
        }
        offset = next;
    }

    saw_real_ttl.then(|| Bytes::from(out))
}

fn negative_ttl(response: &[u8]) -> Option<Duration> {
    let mut offset = skip_questions(response, 12)?;
    let ancount = u16::from_be_bytes([response[6], response[7]]) as usize;
    offset = skip_rrs(response, offset, ancount)?;
    let nscount = u16::from_be_bytes([response[8], response[9]]) as usize;
    let mut min = None;

    for _ in 0..nscount {
        skip_name(response, &mut offset)?;
        if response.len() < offset + 10 {
            return None;
        }
        let rtype = u16::from_be_bytes([response[offset], response[offset + 1]]);
        let rdlength = u16::from_be_bytes([response[offset + 8], response[offset + 9]]) as usize;
        let next = offset.checked_add(10 + rdlength)?;
        if next > response.len() {
            return None;
        }
        if rtype == 6 && rdlength >= 4 {
            let ttl = u32::from_be_bytes([
                response[offset + 4],
                response[offset + 5],
                response[offset + 6],
                response[offset + 7],
            ]);
            let minimum = u32::from_be_bytes([
                response[next - 4],
                response[next - 3],
                response[next - 2],
                response[next - 1],
            ]);
            let candidate = minimum.min(ttl);
            min = Some(min.map_or(candidate, |current: u32| current.min(candidate)));
        }
        offset = next;
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
        let rtype = u16::from_be_bytes([response[offset], response[offset + 1]]);
        let rdlength = u16::from_be_bytes([response[offset + 8], response[offset + 9]]) as usize;
        let next = offset.checked_add(10 + rdlength)?;
        if next > response.len() {
            return None;
        }
        if rtype != EDNS0_TYPE {
            let ttl = u32::from_be_bytes([
                response[offset + 4],
                response[offset + 5],
                response[offset + 6],
                response[offset + 7],
            ]);
            min = Some(min.map_or(ttl, |current: u32| current.min(ttl)));
        }
        offset = next;
    }

    min.filter(|ttl| *ttl > 0)
        .map(|seconds| Duration::from_secs(seconds as u64))
}

fn skip_questions(response: &[u8], mut offset: usize) -> Option<usize> {
    if response.len() < 12 {
        return None;
    }
    let qdcount = u16::from_be_bytes([response[4], response[5]]) as usize;
    for _ in 0..qdcount {
        skip_name(response, &mut offset)?;
        offset = offset.checked_add(4)?;
        if offset > response.len() {
            return None;
        }
    }
    Some(offset)
}

/// Returns (DO bit, fingerprint of all OPT state). Non-OPT additional records
/// in a query are conservatively treated as non-cacheable.
fn edns_state(response: &[u8], mut offset: usize) -> Option<(bool, u64)> {
    let ancount = u16::from_be_bytes([response[6], response[7]]) as usize;
    let nscount = u16::from_be_bytes([response[8], response[9]]) as usize;
    offset = skip_rrs(response, offset, ancount + nscount)?;
    let arcount = u16::from_be_bytes([response[10], response[11]]) as usize;
    let mut do_bit = false;
    let mut hasher = AHasher::default();
    let mut saw_opt = false;

    for _ in 0..arcount {
        skip_name(response, &mut offset)?;
        if response.len() < offset + 10 {
            return None;
        }
        let rtype = u16::from_be_bytes([response[offset], response[offset + 1]]);
        let class = u16::from_be_bytes([response[offset + 2], response[offset + 3]]);
        let pseudo_ttl = u32::from_be_bytes([
            response[offset + 4],
            response[offset + 5],
            response[offset + 6],
            response[offset + 7],
        ]);
        let rdlength = u16::from_be_bytes([response[offset + 8], response[offset + 9]]) as usize;
        let rdata_start = offset + 10;
        let next = rdata_start.checked_add(rdlength)?;
        if next > response.len() || rtype != EDNS0_TYPE {
            return None;
        }

        saw_opt = true;
        do_bit |= pseudo_ttl & 0x0000_8000 != 0;
        hasher.write_u16(class);
        hasher.write_u32(pseudo_ttl);
        hasher.write(&response[rdata_start..next]);
        offset = next;
    }

    Some((do_bit, if saw_opt { hasher.finish() } else { 0 }))
}

fn skip_rrs(response: &[u8], mut offset: usize, count: usize) -> Option<usize> {
    for _ in 0..count {
        skip_name(response, &mut offset)?;
        if response.len() < offset + 10 {
            return None;
        }
        let rdlength = u16::from_be_bytes([response[offset + 8], response[offset + 9]]) as usize;
        offset = offset.checked_add(10 + rdlength)?;
        if offset > response.len() {
            return None;
        }
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
            if *offset + 1 >= response.len() {
                return None;
            }
            *offset += 2;
            return Some(());
        }
        *offset += 1;
        *offset = (*offset).checked_add(len as usize)?;
        if *offset > response.len() {
            return None;
        }
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

    fn add_opt(msg: &mut Vec<u8>, do_bit: bool, option: Option<(u16, &[u8])>) {
        msg[10..12].copy_from_slice(&1u16.to_be_bytes());
        msg.push(0);
        msg.extend_from_slice(&EDNS0_TYPE.to_be_bytes());
        msg.extend_from_slice(&1232u16.to_be_bytes());
        msg.extend_from_slice(&(if do_bit { 0x0000_8000u32 } else { 0 }).to_be_bytes());
        let rdlength = option.map_or(0usize, |(_, value)| 4 + value.len());
        msg.extend_from_slice(&(rdlength as u16).to_be_bytes());
        if let Some((code, value)) = option {
            msg.extend_from_slice(&code.to_be_bytes());
            msg.extend_from_slice(&(value.len() as u16).to_be_bytes());
            msg.extend_from_slice(value);
        }
    }

    fn build_response(name: &str, qtype: u16, ttl: u32, rdata: &[u8], rcode: u16) -> Vec<u8> {
        let mut msg = build_query(name, qtype, 1, true, false);
        msg[2] |= 0x80;
        msg[3] = (msg[3] & 0xF0) | (rcode as u8 & 0x0F);
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
        assert_ne!(cache_key_for_query(&key_a), cache_key_for_query(&key_aaaa));
    }

    #[test]
    fn dnssec_and_edns_options_are_isolated() {
        let mut plain = build_query("example.com", 1, 1, true, false);
        add_opt(&mut plain, false, None);
        let mut dnssec = build_query("example.com", 1, 1, true, false);
        add_opt(&mut dnssec, true, None);
        let mut ecs = build_query("example.com", 1, 1, true, false);
        add_opt(&mut ecs, false, Some((8, &[0, 1, 2, 3])));

        let plain = dns_query_key(&plain).unwrap();
        let dnssec = dns_query_key(&dnssec).unwrap();
        let ecs = dns_query_key(&ecs).unwrap();
        assert!(!plain.do_bit);
        assert!(dnssec.do_bit);
        assert_ne!(cache_key_for_query(&plain), cache_key_for_query(&dnssec));
        assert_ne!(cache_key_for_query(&plain), cache_key_for_query(&ecs));
    }

    #[test]
    fn ages_ttl_and_rewrites_transaction_id() {
        let response = Bytes::from(build_response("example.com", 1, 120, &[192, 0, 2, 1], 0));
        let stored_at = Instant::now() - Duration::from_secs(30);
        let aged =
            age_dns_response_for_query(&response, stored_at, Instant::now(), 0x1234).unwrap();
        assert_eq!(&aged[0..2], &[0x12, 0x34]);
        let mut offset = skip_questions(&aged, 12).unwrap();
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
    fn opt_flags_are_not_aged_or_used_as_ttl() {
        let mut response = build_response("example.com", 1, 120, &[192, 0, 2, 1], 0);
        add_opt(&mut response, true, None);
        assert!(matches!(
            dns_cacheable(&response, &DnsCachePolicy::default()),
            DnsCacheDecision::Cacheable { .. }
        ));
        let response = Bytes::from(response);
        let stored_at = Instant::now() - Duration::from_secs(30);
        let aged =
            age_dns_response_for_query(&response, stored_at, Instant::now(), 0x2222).unwrap();

        let mut offset = skip_questions(&aged, 12).unwrap();
        offset = skip_rrs(&aged, offset, 1).unwrap();
        skip_name(&aged, &mut offset).unwrap();
        assert_eq!(
            u16::from_be_bytes([aged[offset], aged[offset + 1]]),
            EDNS0_TYPE
        );
        assert_eq!(
            u32::from_be_bytes([
                aged[offset + 4],
                aged[offset + 5],
                aged[offset + 6],
                aged[offset + 7],
            ]),
            0x0000_8000
        );
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
        rdata.extend(encode_name("host.example"));
        rdata.extend_from_slice(&1u32.to_be_bytes());
        rdata.extend_from_slice(&3600u32.to_be_bytes());
        rdata.extend_from_slice(&86400u32.to_be_bytes());
        rdata.extend_from_slice(&120u32.to_be_bytes());
        rdata.extend_from_slice(&60u32.to_be_bytes());
        msg.extend_from_slice(&(rdata.len() as u16).to_be_bytes());
        msg.extend_from_slice(&rdata);
        assert!(matches!(
            dns_cacheable(&msg, &DnsCachePolicy::default()),
            DnsCacheDecision::Cacheable { .. }
        ));
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
