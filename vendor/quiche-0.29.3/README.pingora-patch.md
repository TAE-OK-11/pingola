# Local quiche patch

Source: crates.io `quiche` 0.29.3 (BSD-2-Clause).

- Local change: `boring` dependency from `4.3` to `5`.
- Reason: Pingora 0.9 / `pingora-boringssl` require Cloudflare `boring` 5.x;
  this project must keep a single `boring`/`boring-sys` version shared with HTTP/3.
