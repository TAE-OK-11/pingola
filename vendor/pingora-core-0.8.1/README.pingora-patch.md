# Local Pingora core patch

This directory is the source of `pingora-core` 0.8.1 from crates.io with a
small set of documented local changes.

- Upstream package: `pingora-core` 0.8.1
- License: Apache-2.0 (`LICENSE` in this directory)
- Local change: `prometheus = "0.13"` to `prometheus = "0.14"`
- Reason: Prometheus 0.13 selects `protobuf` 2.28.0, affected by
  RUSTSEC-2024-0437. Prometheus 0.14 requires patched protobuf 3.7.2 or newer.
- Local change: an opt-in `HttpPeer::cache_reuse_hash()` method.
- Reason: fully prepared immutable route peers otherwise hash their address,
  SNI and pool options repeatedly for every connection-pool operation. Mutable
  peers retain upstream behavior because the cache defaults to `None`.
- Local change: `serde_yaml` to `serde-saphyr` 0.0.29.
- Reason: remove the deprecated libyaml binding while retaining typed config
  serialization and deserialization.
- Local change: Brotli 3 to Brotli 9 and flate2 zlib-ng to zlib-rs.
- Reason: use one Brotli version across the final binary and select exactly one
  high-performance DEFLATE backend instead of compiling ambiguous backends.
- Local change: use a 16-KiB upstream H1 response buffer and move an exact
  completed body chunk into `Bytes` without copying.
- Reason: the upstream 4-KiB header buffer split a 4096-byte response at
  `4096 - header_len`, causing an extra read/write syscall. 8 KiB covered the
  common API case; 16 KiB keeps typical JSON and small static responses in one
  read while the allocation remains bounded per active upstream response.
  Partial, chunked, overread and downstream body paths retain their original
  semantics. The header storage lives for that response because parsed
  HeaderValue instances share it.
- Local change: parse downstream H1 requests and upstream H1 responses directly
  into the no-case representation used by this proxy.
- Reason: the proxy cloned only semantic request parts before forwarding and
  all response filters use semantic HeaderName lookups. Preserving a second map
  of original field-name spelling therefore allocated and copied every header
  without affecting HTTP semantics. Header values, duplicate ordering and
  case-insensitive lookup are unchanged.
- Local change: store up to 16 temporary H1 parsed-header offsets inline with
  `SmallVec`, spilling to the heap for larger requests and responses.
- Reason: typical proxy traffic no longer performs a separate allocation just
  to transfer zero-copy header offsets. Large requests retain the same
  `MAX_HEADERS = 256` behavior and are covered by a spill-path regression test.
- Local change: parse ordinary downstream requests and upstream responses into
  `MaybeUninit` header arrays through httparse's safe public APIs.
- Reason: the previous hot path initialized all 256 header slots (about 8 KiB)
  for every request and response even though most traffic uses only a handful.
  The upstream `patched_http1` feature retains its initialized buffer because
  its separate unchecked parser does not expose the uninitialized-header API.
- Local change: `response_duplex_one()` on custom downstream sessions.
- Reason: HTTP/3 bodyless responses use the same single-task write path as
  HTTP/1. Avoid constructing a temporary `Vec<HttpTask>` on every response
  chunk for custom sessions.
- Local change: initialize HTTP/1 body read slices before passing them to
  `AsyncReadExt::read`.
- Reason: extending `BytesMut` with `set_len` exposed uninitialized allocation
  bytes through an initialized mutable slice, violating Rust's unsafe-code
  contract on a network-reachable body path.
- Local change: discard the bounded retry-body allocation as soon as buffering
  truncates.
- Reason: a truncated request cannot be replayed, so retaining its contents
  only pins memory until the downstream connection closes. This follows the
  post-0.8.1 Cloudflare Pingora optimization.
- Local change: move filled downstream H1 request body buffers into `Bytes`
  without copying when the chunk is complete.
- Reason: upstream H1 already used `take_completed_body()`; the downstream
  server path still copied every body chunk even when the internal buffer was
  complete. `take_filled_body()` shares the same safety guards.
- Local change: enable `TCP_QUICKACK` alongside `TCP_NODELAY` on Linux TCP
  streams.
- Reason: short proxy responses (health checks, API JSON) avoid delayed-ACK
  stalls on the return path without extra userspace work per connection.
- Local change: tune accepted and upstream TCP sockets with 256-KiB SO_RCVBUF /
  SO_SNDBUF and TCP_NOTSENT_LOWAT for streaming.
- Reason: keep kernel send/receive pipelines fed without extra userspace
  buffering on Navidrome-sized transfers.
- Local change: raise L4 `BufStream` write capacity from 1460 B to 16 KiB.
- Reason: the previous MSS-sized userspace write buffer forced one syscall per
  small TLS record on large fixed responses; 16 KiB batches writes while
  TCP_NODELAY still governs kernel packetization.
- Local change: skip the redundant flush in `HttpSession::finish_body` when the
  upstream request is already fully framed.
- Reason: bodyless GET/HEAD requests use Content-Length 0 and
  `write_request_header` already flushed the wire bytes; completed Content-Length
  bodies already flush inside `write_body`. A second empty flush added latency on
  every keep-alive upstream H1 exchange without changing framing.
- Local change: size `http_req_header_to_wire` from the request instead of a
  fixed 512-byte guess.
- Reason: proxy requests carry Host plus several X-Forwarded-* headers; one
  correctly sized allocation avoids BytesMut growth on the upstream write path.
- Local change: zero-copy streaming H1 body prefixes (≥16 KiB) via buffer freeze
  and rotation, in addition to completed-body moves.
- Reason: large Content-Length / until-close chunks previously always
  `copy_from_slice`'d even when `BufRef` covered a leading contiguous range.
  Freezing that prefix and rotating the read buffer removes the memcpy on the
  common streaming path; small chunks keep the copy path because replacing the
  64 KiB read buffer would cost more than copying them.

Remove dependency patches after a released Pingora version adopts equivalent
versions. Re-evaluate the reuse-hash cache whenever `HttpPeer` changes.
