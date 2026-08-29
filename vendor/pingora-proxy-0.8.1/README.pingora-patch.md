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
  downstream sessions (HTTP/3).
- Reason: QUIC streams talk to HTTP/1 origins through the same proxy_1to1
  path; requiring an HTTP/1 downstream would skip the fast path for H3.
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
