# Local Pingora HTTP patch

This directory is the source of `pingora-http` 0.9.0 from crates.io with a
small documented local change.

- Upstream package: `pingora-http` 0.9.0
- License: Apache-2.0 (`LICENSE` in this directory)
- Local change: clone `http::request::Parts` and `http::response::Parts`
  directly instead of rebuilding them through a validated builder and then
  replacing its headers and extensions.
- Reason: `http` provides an exact `Clone` implementation for both parts
  types. Direct cloning preserves all fields and extensions while avoiding
  redundant builder construction and validation on each proxied request.
- Local change: add `insert_typed_header()` for `RequestHeader` and
  `ResponseHeader`.
- Reason: the generic `insert_header()` rebuilds a `HeaderName` from case
  bytes on every call. Proxy hot paths already hold parsed names and values,
  so the typed insert is a single `HeaderMap` write when case is not kept.
- Local change: add `ResponseHeader::set_headers()` to move an owned
  `HeaderMap` onto a response without `DerefMut`.
- Reason: Pingora 0.9 removed `DerefMut` on header types; HTTP/3 upstream
  decoding already builds a `HeaderMap` that should be moved once.

Remove this dependency patch after a released Pingora version adopts an
equivalent implementation.
