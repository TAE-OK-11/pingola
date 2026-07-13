# Pingola

Pingola is a Rust reverse proxy built with Cloudflare Pingora. It is designed to
replace the Nginx gateway used for PiKKY, Navidrome, Vaultwarden, CouchDB and
AdGuard Home on a 1 vCPU / 1 GB host.

The checked-in configuration mirrors the original host names, upstreams,
trusted Cloudflare networks and request body limits. TLS certificates and
private keys are runtime mounts and are never included in the image.

> Status: configuration model complete; proxy runtime and container are under
> active implementation.

## Configuration check

```bash
cargo run -- --config config/pingola.yaml --check
```

TLS cryptography is provided by AWS-LC through rustls. The process installs the
AWS-LC provider explicitly before Pingora creates any listeners. Downstream
HTTP/1.1 and HTTP/2 are supported; HTTP/3/QUIC is not advertised by this
gateway.
