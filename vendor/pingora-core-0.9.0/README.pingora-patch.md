# Local Pingora core patch

This directory is the source of `pingora-core` 0.9.0 from crates.io with a
small set of documented local changes.

- Upstream package: `pingora-core` 0.9.0
- License: Apache-2.0 (`LICENSE` in this directory)
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
- Local change: add `smallvec` (used by H1 header offset storage).
- Reason: upstream 0.9.0 Cargo.toml omitted the dependency while the H1 parser
  still stores temporary header offsets in `SmallVec`.
- Local change: use a 16-KiB upstream H1 response buffer and move an exact
  completed body chunk into `Bytes` without copying.
- Reason: the upstream 4-KiB header buffer split a 4096-byte response at
  `4096 - header_len`, causing an extra read/write syscall. 8 KiB covered the
  common API case; 16 KiB keeps typical JSON and small static responses in one
  read while the allocation remains bounded per active upstream response.
- Local change: parse downstream H1 requests and upstream H1 responses directly
  into the no-case representation used by this proxy.
- Local change: store up to 16 temporary H1 parsed-header offsets inline with
  `SmallVec`, spilling to the heap for larger requests and responses.
- Local change: parse ordinary downstream requests and upstream responses into
  `MaybeUninit` header arrays through httparse's safe public APIs.
- Local change: `response_duplex_one()` on custom downstream sessions.
- Local change: initialize HTTP/1 body read slices before passing them to
  `AsyncReadExt::read`.
- Local change: discard the bounded retry-body allocation as soon as buffering
  truncates.
- Local change: move filled downstream H1 request body buffers into `Bytes`
  without copying when the chunk is complete.
- Local change: enable `TCP_QUICKACK` alongside `TCP_NODELAY` on Linux TCP
  streams.
- Local change: tune accepted and upstream TCP sockets with 256-KiB SO_RCVBUF /
  SO_SNDBUF and TCP_NOTSENT_LOWAT for streaming.
- Local change: raise L4 `BufStream` write capacity from 1460 B to 16 KiB.
- Local change: skip the redundant flush in `HttpSession::finish_body` when the
  upstream request is already fully framed.
- Local change: size `http_req_header_to_wire` from the request instead of a
  fixed 512-byte guess.
- Local change: zero-copy streaming H1 body prefixes (≥16 KiB) via buffer freeze
  and rotation, in addition to completed-body moves.

Remove dependency patches after a released Pingora version adopts equivalent
versions. Re-evaluate the reuse-hash cache whenever `HttpPeer` changes.
