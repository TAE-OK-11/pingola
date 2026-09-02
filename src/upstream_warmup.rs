//! Background TCP/TLS warmup for H1/H2 upstreams at process start.
//!
//! HTTP/3 origins use `http3_warmup` on the QUIC pool instead. This path only
//! performs a TLS handshake so the first proxied request avoids cold TCP+TLS
//! setup latency (for example AdGuard DoH on a separate TLS port).

use std::net::{TcpStream, ToSocketAddrs};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use boring::ssl::{SslConnector, SslMethod, SslVerifyMode};
use log::{info, warn};

use crate::config::{RuntimeConfig, UpstreamConfig, UpstreamProtocol};

pub fn spawn(runtime: Arc<RuntimeConfig>, handle: &tokio::runtime::Handle) {
    for (name, upstream) in &runtime.config.upstreams {
        if !upstream.warmup_on_start || upstream.protocol.uses_http3() {
            continue;
        }
        if !upstream.tls {
            continue;
        }
        let name = name.clone();
        let upstream = upstream.clone();
        let trust_anchor = runtime.config.server.certificate.clone();
        handle.spawn(async move {
            match warmup_tls_upstream(&name, &upstream, trust_anchor).await {
                Ok(()) => info!("upstream TCP/TLS warmup completed: name={name}"),
                Err(error) => warn!("upstream TCP/TLS warmup failed: name={name} error={error:#}"),
            }
        });
    }
}

async fn warmup_tls_upstream(
    name: &str,
    upstream: &UpstreamConfig,
    trust_anchor: Option<PathBuf>,
) -> Result<()> {
    let addr = upstream
        .address
        .to_socket_addrs()
        .with_context(|| format!("upstream address resolution failed: name={name}"))?
        .next()
        .ok_or_else(|| anyhow::anyhow!("upstream {name} resolved to no addresses"))?;
    let sni = upstream
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
        .to_string();
    let verify_certificate = upstream.verify_certificate;
    let connect_timeout = Duration::from_secs(upstream.connect_timeout_seconds);
    let alpn: Vec<Vec<u8>> = match upstream.protocol {
        UpstreamProtocol::Auto if upstream.tls => vec![b"h2".to_vec(), b"http/1.1".to_vec()],
        UpstreamProtocol::Auto | UpstreamProtocol::Http1 => vec![b"http/1.1".to_vec()],
        UpstreamProtocol::Http2 => vec![b"h2".to_vec()],
        UpstreamProtocol::Http3 | UpstreamProtocol::Http3Preferred => vec![b"http/1.1".to_vec()],
    };

    tokio::task::spawn_blocking(move || {
        blocking_tls_handshake(
            addr,
            &sni,
            verify_certificate,
            trust_anchor,
            &alpn,
            connect_timeout,
        )
    })
    .await
    .context("upstream warmup task join failed")?
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
