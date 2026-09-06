# Local Pingora proxy patch

This directory is the source of `pingora-proxy` 0.8.1 from crates.io with a
small set of documented local changes.

- Upstream package: `pingora-proxy` 0.8.1
- License: Apache-2.0 (`LICENSE` in this directory)
- Local change: `precomputed_upstream_peer()` on `ProxyHttp`.
- Reason: immutable prepared peers can be returned by reference instead of
  boxing a clone on every request.
- Local change: HTTP/1 bodyless GET/HEAD fast path.
- Reason: empty GET/HEAD requests do not need per-request mpsc channels, retry
  buffers, or cache/range state.
- Local change: HTTP/1 bodyless GET/HEAD fast path also applies to custom
  downstream sessions (HTTP/3) and HTTP/2 downstream sessions.
- Reason: QUIC streams talk to HTTP/1 origins through the same proxy_1to1
  path; requiring an HTTP/1 downstream would skip the fast path for H3. Public
  HTTP/2 clients also reach HTTP/1 origins through `proxy_to_h1_upstream()`;
  without this extension every H2 GET/HEAD paid for duplex mpsc channels,
  retry buffers, and extra `select!` work on large response bodies.
- Local change: HTTP/2 bodyless GET/HEAD fast path using the same opt-in.
- Reason: public TLS traffic is dominated by HTTP/2 GET/HEAD. Skipping the
  duplex channel pair and retry buffer matches the HTTP/1 fast path.
- Local change: clone only semantic HTTP/2 request parts (`as_owned_parts`).
- Reason: HTTP/2 never needs the downstream header-case map on the upstream
  request, so hop-by-hop mutations should update one map.
- Local change: skip HTTP/1 chunked `Transfer-Encoding` injection when the
  downstream session is custom (HTTP/3).
- Reason: QUIC responses do not use HTTP/1 framing; adding and then stripping
  hop-by-hop headers wasted work on the bodyless and duplex proxy paths.
- Local change: skip upstream compression `request_filter()` when the ctx is
  disabled (level 0).
- Reason: bodyless proxy traffic pays for accept-encoding parsing on every
  upstream request even when compression is off.
- Local change: optional downstream polling in the HTTP/1 bodyless fast path.
- Reason: long-lived streams need disconnect propagation; short fixed-size proxy
  responses can skip a `select!` branch on every upstream body chunk.
- Local change: HTTP/2 bodyless fast path reuses the same downstream poll opt-in
  and skips HTTP/1 chunked framing when the client is already HTTP/2.
- Local change: skip `upstream_compression.response_filter` on the bodyless H1
  path when compression is disabled (the common case).
- Reason: `response_filter` already returns immediately when disabled, but the
  call still pays for an enabled-state check through a trait object on every
  response task; hoist the check once per request.
