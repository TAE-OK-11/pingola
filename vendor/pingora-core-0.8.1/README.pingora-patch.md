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
- Local change: Brotli 3 to Brotli 8 and flate2 zlib-ng to zlib-rs.
- Reason: use one Brotli version across the final binary and select exactly one
  high-performance DEFLATE backend instead of compiling ambiguous backends.
- Local change: use an 8-KiB upstream H1 response buffer and move an exact
  completed body chunk into `Bytes` without copying.
- Reason: the upstream 4-KiB header buffer split a 4096-byte response at
  `4096 - header_len`, causing an extra read/write syscall, while
  `read_body_bytes` copied even a complete owned body buffer. The 8-KiB
  allocation is bounded per active upstream response; partial, chunked,
  overread and downstream body paths retain their original semantics. The
  header storage lives for that response because parsed HeaderValue instances
  share it.

Remove dependency patches after a released Pingora version adopts equivalent
versions. Re-evaluate the reuse-hash cache whenever `HttpPeer` changes.
