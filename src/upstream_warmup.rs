//! Background TCP/TLS and H2C warmup for upstreams at process start.
//!
//! HTTP/3 origins use `http3_warmup` on the QUIC pool instead. This path
//! warms TLS session setup for HTTPS origins and the HTTP/2 connection
//! preface for plaintext `http2`/`grpc` origins so the first proxied request
//! does not pay a cold handshake. It runs on its own thread and does not
//! depend on the HTTP/3 runtime.

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use boring::ssl::{SslConnector, SslMethod, SslVerifyMode};
use log::{info, warn};

use crate::config::{RuntimeConfig, UpstreamConfig, UpstreamProtocol};

/// HTTP/2 connection preface. RFC 9113 §3.4.
const H2_CLIENT_PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";
/// Empty SETTINGS frame: 9-byte header, type 0x4, stream 0.
const H2_EMPTY_SETTINGS: &[u8] = &[0, 0, 0, 0x04, 0, 0, 0, 0, 0];

pub fn spawn(runtime: Arc<RuntimeConfig>) {
    let trust_anchor = runtime.config.server.certificate.clone();
    std::thread::Builder::new()
        .name("upstream-warmup".into())
        .spawn(move || {
            for (name, upstream) in &runtime.config.upstreams {
                if !upstream.warmup_on_start || upstream.protocol.uses_http3() {
                    continue;
                }
                match warmup_upstream(name, upstream, trust_anchor.as_ref()) {
                    Ok(()) => info!("upstream warmup completed: name={name}"),
                    Err(error) => warn!("upstream warmup failed: name={name} error={error:#}"),
                }
            }
        })
        .ok();
}

fn warmup_upstream(
    name: &str,
    upstream: &UpstreamConfig,
    trust_anchor: Option<&PathBuf>,
) -> Result<()> {
    if upstream.tls {
        warmup_tls_upstream(name, upstream, trust_anchor.cloned())
    } else if matches!(
        upstream.protocol,
        UpstreamProtocol::Http2 | UpstreamProtocol::Grpc
    ) {
        warmup_h2c_upstream(name, upstream)
    } else {
        Ok(())
    }
}

fn warmup_tls_upstream(
    name: &str,
    upstream: &UpstreamConfig,
    trust_anchor: Option<PathBuf>,
) -> Result<()> {
    let addr = resolve_upstream(name, upstream)?;
    let sni = upstream_sni(upstream);
    let alpn: Vec<Vec<u8>> = match upstream.protocol {
        UpstreamProtocol::Auto if upstream.tls => vec![b"h2".to_vec(), b"http/1.1".to_vec()],
        UpstreamProtocol::Auto | UpstreamProtocol::Http1 => vec![b"http/1.1".to_vec()],
        UpstreamProtocol::Http2 | UpstreamProtocol::Grpc => vec![b"h2".to_vec()],
        UpstreamProtocol::Http3 | UpstreamProtocol::Http3Preferred => vec![b"http/1.1".to_vec()],
    };
    blocking_tls_handshake(
        addr,
        &sni,
        upstream.verify_certificate,
        trust_anchor,
        &alpn,
        Duration::from_secs(upstream.connect_timeout_seconds),
    )
}

fn warmup_h2c_upstream(name: &str, upstream: &UpstreamConfig) -> Result<()> {
    let addr = resolve_upstream(name, upstream)?;
    let timeout = Duration::from_secs(upstream.connect_timeout_seconds.max(1));
    let mut stream = TcpStream::connect_timeout(&addr, timeout)
        .with_context(|| format!("TCP connect failed for H2C warmup to {addr}"))?;
    stream.set_nodelay(true).ok();
    stream.set_write_timeout(Some(timeout)).ok();
    stream.set_read_timeout(Some(timeout)).ok();
    stream
        .write_all(H2_CLIENT_PREFACE)
        .and_then(|_| stream.write_all(H2_EMPTY_SETTINGS))
        .with_context(|| format!("failed to write HTTP/2 preface during warmup to {addr}"))?;
    // Read the 9-byte server SETTINGS header so the origin finishes its
    // preface exchange. Ignore the payload; this connection is discarded.
    let mut header = [0_u8; 9];
    stream
        .read_exact(&mut header)
        .with_context(|| format!("H2C origin did not complete HTTP/2 preface at {addr}"))?;
    if header[3] != 0x04 {
        bail!(
            "H2C origin at {addr} sent frame type {} instead of SETTINGS",
            header[3]
        );
    }
    Ok(())
}

fn resolve_upstream(name: &str, upstream: &UpstreamConfig) -> Result<std::net::SocketAddr> {
    upstream
        .address
        .to_socket_addrs()
        .with_context(|| format!("upstream address resolution failed: name={name}"))?
        .next()
        .ok_or_else(|| anyhow::anyhow!("upstream {name} resolved to no addresses"))
}

fn upstream_sni(upstream: &UpstreamConfig) -> String {
    upstream
        .sni
        .as_deref()
        .filter(|value| !value.is_empty())
        .or_else(|| {
            upstream
                .address
                .rsplit_once(':')
                .map(|(host, _)| host.trim_matches(['[', ']']))
        })
        .unwrap_or("localhost")
        .to_string()
}

fn blocking_tls_handshake(
    addr: std::net::SocketAddr,
    sni: &str,
    verify_peer: bool,
    trust_anchor: Option<PathBuf>,
    alpn: &[Vec<u8>],
    connect_timeout: Duration,
) -> Result<()> {
    let stream = TcpStream::connect_timeout(&addr, connect_timeout)
        .with_context(|| format!("TCP connect failed for warmup to {addr}"))?;
    stream.set_nodelay(true).ok();

    let mut builder = SslConnector::builder(SslMethod::tls())
        .context("failed to create TLS connector for warmup")?;
    if verify_peer {
        builder
            .set_default_verify_paths()
            .context("failed to load default trust roots for warmup")?;
        if let Some(anchor) = trust_anchor.as_ref() {
            builder.set_ca_file(anchor).with_context(|| {
                format!("failed to load warmup trust anchor {}", anchor.display())
            })?;
        }
        builder.set_verify(SslVerifyMode::PEER);
    } else {
        builder.set_verify(SslVerifyMode::NONE);
    }
    builder
        .set_alpn_protos(&encode_alpn(alpn))
        .context("failed to configure warmup ALPN")?;
    let connector = builder.build();
    let _ssl_stream = connector
        .connect(sni, stream)
        .with_context(|| format!("TLS handshake failed during warmup to {addr}"))?;
    Ok(())
}

fn encode_alpn(protocols: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    for protocol in protocols {
        out.push(protocol.len() as u8);
        out.extend_from_slice(protocol.as_slice());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;

    #[test]
    fn h2c_warmup_sends_preface_and_accepts_settings() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut preface = [0_u8; H2_CLIENT_PREFACE.len()];
            stream.read_exact(&mut preface).unwrap();
            assert_eq!(&preface, H2_CLIENT_PREFACE);
            let mut settings = [0_u8; H2_EMPTY_SETTINGS.len()];
            stream.read_exact(&mut settings).unwrap();
            assert_eq!(&settings, H2_EMPTY_SETTINGS);
            stream.write_all(H2_EMPTY_SETTINGS).unwrap();
        });

        let yaml = format!("address: {addr}\nprotocol: grpc\nconnect_timeout_seconds: 2");
        let upstream: crate::config::UpstreamConfig = serde_saphyr::from_str(&yaml).unwrap();
        warmup_h2c_upstream("navidrome_grpc", &upstream).unwrap();
        server.join().unwrap();
    }
}
