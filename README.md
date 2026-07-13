# Pingola

Pingola is a Rust reverse proxy built with Cloudflare Pingora. It is designed to
replace the Nginx gateway used for PiKKY, Navidrome, Vaultwarden, CouchDB and
AdGuard Home on a 1 vCPU / 1 GB host.

The checked-in configuration mirrors the original host names, upstreams,
trusted Cloudflare networks and request body limits. TLS certificates and
private keys are runtime mounts and are never included in the image.

> Status: proxy runtime complete; container and end-to-end deployment checks
> are under active implementation.

The runtime includes:

- one Pingora worker service for both port 80 and 443 listeners;
- TLS 1.3 cipher suites backed by AWS-LC and HTTP/2 with 32 streams;
- strict host allowlisting and trusted-proxy-aware client IP handling;
- per-IP request and active-stream limits matching the Nginx zones;
- route-specific streaming, WebSocket, CouchDB and DoH timeouts;
- request body limits, upstream header sanitization and security headers;
- bounded static-file LRU caching with gzip, Brotli and Zstd negotiation.

## Configuration check

```bash
cargo run -- --config config/pingola.yaml --check
```

TLS cryptography is provided by AWS-LC through rustls. The process installs the
AWS-LC provider explicitly before Pingora creates any listeners. Downstream
HTTP/1.1 and HTTP/2 are supported; HTTP/3/QUIC is not advertised by this
gateway.
