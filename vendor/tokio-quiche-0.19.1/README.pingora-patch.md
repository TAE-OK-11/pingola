# Local tokio-quiche patch

Source: crates.io `tokio-quiche` 0.19.1 (BSD-2-Clause).

- Local change: `boring` dependency from `4.3` to `5`.
- Reason: share Cloudflare `boring` 5.x with Pingora 0.9 and patched `quiche`.
