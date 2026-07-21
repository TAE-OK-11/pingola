# Local Pingora HTTP patch

This directory is the source of `pingora-http` 0.8.1 from crates.io with a
small documented local change.

- Upstream package: `pingora-http` 0.8.1
- License: Apache-2.0 (`LICENSE` in this directory)
- Local change: clone `http::request::Parts` and `http::response::Parts`
  directly instead of rebuilding them through a validated builder and then
  replacing its headers and extensions.
- Reason: `http` 1.4 provides an exact `Clone` implementation for both parts
  types. Direct cloning preserves all fields and extensions while avoiding
  redundant builder construction and validation on each proxied request.

Remove this dependency patch after a released Pingora version adopts an
equivalent implementation.
