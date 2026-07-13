# Local Pingora core patch

This directory is the unmodified source of `pingora-core` 0.8.1 from
crates.io, except for its Prometheus dependency declaration.

- Upstream package: `pingora-core` 0.8.1
- License: Apache-2.0 (`LICENSE` in this directory)
- Local change: `prometheus = "0.13"` to `prometheus = "0.14"`
- Reason: Prometheus 0.13 selects `protobuf` 2.28.0, affected by
  RUSTSEC-2024-0437. Prometheus 0.14 requires patched protobuf 3.7.2 or newer.

No Pingora source code or public API is changed. Remove this patch and the
root `[patch.crates-io]` entry after a released Pingora version adopts a
patched Prometheus dependency or moves the metrics application out of core.
