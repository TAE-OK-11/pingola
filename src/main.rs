mod allocator;
mod config;
mod content_encoding;
mod gateway;
mod limits;
mod preflight;
mod static_files;

use std::fs::{self, Permissions};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use clap::Parser;
use cloudflare_pingora::apps::HttpServerOptions;
use cloudflare_pingora::listeners::tls::TlsSettings;
use cloudflare_pingora::listeners::TcpSocketOptions;
use cloudflare_pingora::protocols::http::v2::server::default_h2_options;
use cloudflare_pingora::protocols::TcpKeepalive;
use cloudflare_pingora::proxy::ProxyServiceBuilder;
use cloudflare_pingora::server::configuration::ServerConf;
use cloudflare_pingora::server::Server;
use log::info;

use crate::config::RuntimeConfig;
use crate::gateway::Gateway;
use crate::preflight::check_runtime;

#[cfg(all(feature = "tls-aws-lc", feature = "tls-boringssl"))]
compile_error!("select exactly one TLS provider: tls-aws-lc or tls-boringssl");
#[cfg(not(any(feature = "tls-aws-lc", feature = "tls-boringssl")))]
compile_error!("a TLS provider is required: tls-aws-lc or tls-boringssl");

const PRIMARY_CONFIG: &str = "/etc/pingora/pingora.yaml";
const LEGACY_CONFIG: &str = "/etc/pingola/pingola.yaml";

#[derive(Debug, Parser)]
#[command(version, about = "JBS Pingora reverse proxy")]
struct Cli {
    /// Gateway configuration file.
    #[arg(short, long, env = "PINGORA_CONFIG")]
    config: Option<PathBuf>,

    /// Validate schema, files, permissions, PEM, and static roots without binding.
    #[arg(long)]
    check: bool,

    /// Also bind every configured TCP listener simultaneously during validation.
    #[arg(long)]
    check_bind: bool,

    /// Probe health using auto config, unix:/path, tcp:address, or a legacy bare address.
    #[arg(
        long,
        value_name = "TARGET",
        num_args = 0..=1,
        default_missing_value = "auto",
        env = "PINGORA_HEALTH_TARGET"
    )]
    healthcheck: Option<String>,

    /// Print the linked allocator and optional current statistics, then exit.
    #[arg(long)]
    allocator_info: bool,
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let cli = Cli::parse();

    if cli.allocator_info {
        println!("{}", allocator::summary(true)?);
        return Ok(());
    }

    let config_path = resolve_config_path(cli.config)?;
    if let Some(target) = cli.healthcheck.as_deref() {
        return run_healthcheck(target, &config_path);
    }

    install_aws_lc_tls13_provider().context("TLS provider initialization failed")?;
    let runtime =
        Arc::new(RuntimeConfig::load(&config_path).with_context(|| {
            format!("configuration validation failed: {}", config_path.display())
        })?);

    if cli.check || cli.check_bind {
        println!(
            "[ok] schema validation: path={} hosts={} upstreams={} listeners={}",
            config_path.display(),
            runtime.config.hosts.len(),
            runtime.config.upstreams.len(),
            runtime.config.server.http_listen.len() + runtime.config.server.https_listen.len()
        );
        let report = check_runtime(&runtime, cli.check_bind);
        report.print();
        if !report.is_ok() {
            bail!("runtime preflight validation failed");
        }
        return Ok(());
    }

    prepare_health_socket_directory(&runtime.config.server.health_socket)?;
    let report = check_runtime(&runtime, true);
    if !report.is_ok() {
        report.print();
        bail!("startup preflight failed; Pingora was not started");
    }
    info!("startup preflight passed ({} checks)", report.items.len());
    if allocator::environment_requests_stats() {
        info!("{}", allocator::summary(true)?);
    } else {
        info!("{}", allocator::summary(false)?);
    }

    run(runtime)
}

fn resolve_config_path(cli_path: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(path) = cli_path {
        if path == Path::new(LEGACY_CONFIG) {
            eprintln!(
                "warning: {LEGACY_CONFIG} is deprecated; migrate to {PRIMARY_CONFIG} (legacy support will be removed after one release)"
            );
        }
        return Ok(path);
    }

    if let Some(path) = std::env::var_os("PINGOLA_CONFIG") {
        eprintln!(
            "warning: PINGOLA_CONFIG is deprecated; use PINGORA_CONFIG (legacy support will be removed after one release)"
        );
        return Ok(PathBuf::from(path));
    }
    if Path::new(PRIMARY_CONFIG).exists() {
        return Ok(PathBuf::from(PRIMARY_CONFIG));
    }
    if Path::new(LEGACY_CONFIG).exists() {
        eprintln!(
            "warning: using deprecated config {LEGACY_CONFIG}; migrate to {PRIMARY_CONFIG} (legacy support will be removed after one release)"
        );
        return Ok(PathBuf::from(LEGACY_CONFIG));
    }
    Ok(PathBuf::from(PRIMARY_CONFIG))
}

fn prepare_health_socket_directory(socket: &Path) -> Result<()> {
    let parent = socket
        .parent()
        .ok_or_else(|| anyhow!("health socket has no parent: {}", socket.display()))?;
    fs::create_dir_all(parent).with_context(|| {
        format!(
            "failed to create health socket directory {}",
            parent.display()
        )
    })
}

fn run_healthcheck(target: &str, config_path: &Path) -> Result<()> {
    let target = if target == "auto" {
        let runtime = RuntimeConfig::load(config_path).with_context(|| {
            format!(
                "healthcheck could not load configuration {}",
                config_path.display()
            )
        })?;
        format!("unix:{}", runtime.config.server.health_socket.display())
    } else {
        target.to_string()
    };

    let timeout = Duration::from_secs(2);
    if let Some(path) = target.strip_prefix("unix:") {
        let mut stream = UnixStream::connect(path).with_context(|| {
            format!("healthcheck failed to connect to actual target unix:{path}")
        })?;
        stream.set_read_timeout(Some(timeout))?;
        stream.set_write_timeout(Some(timeout))?;
        probe_health(&mut stream, &target)
    } else {
        let address = target.strip_prefix("tcp:").unwrap_or(&target);
        let socket = resolve_health_address(address)?;
        let mut stream = TcpStream::connect_timeout(&socket, timeout).with_context(|| {
            format!("healthcheck failed to connect to actual target tcp:{address}")
        })?;
        stream.set_read_timeout(Some(timeout))?;
        stream.set_write_timeout(Some(timeout))?;
        probe_health(&mut stream, &format!("tcp:{address}"))
    }
}

fn resolve_health_address(address: &str) -> Result<SocketAddr> {
    address
        .to_socket_addrs()
        .with_context(|| format!("invalid healthcheck target tcp:{address}"))?
        .next()
        .ok_or_else(|| anyhow!("healthcheck target did not resolve: tcp:{address}"))
}

fn probe_health(stream: &mut (impl Read + Write), target: &str) -> Result<()> {
    stream
        .write_all(
            b"GET /pingora-health HTTP/1.1\r\nHost: health.invalid\r\nConnection: close\r\n\r\n",
        )
        .with_context(|| format!("healthcheck write failed for actual target {target}"))?;
    let mut response = Vec::with_capacity(512);
    let mut chunk = [0_u8; 512];
    while response.len() < 4096 && !response.windows(4).any(|window| window == b"\r\n\r\n") {
        let bytes = stream
            .read(&mut chunk)
            .with_context(|| format!("healthcheck read failed for actual target {target}"))?;
        if bytes == 0 {
            break;
        }
        response.extend_from_slice(&chunk[..bytes]);
    }
    let response = String::from_utf8_lossy(&response);
    let status_ok = response.starts_with("HTTP/1.1 204 ") || response.starts_with("HTTP/1.0 204 ");
    let product_ok = response.lines().any(|line| {
        line.trim_end_matches('\r')
            .eq_ignore_ascii_case("x-proxy-product: Pingora")
    });
    if status_ok && product_ok {
        Ok(())
    } else {
        let status = response.lines().next().unwrap_or("empty response");
        Err(anyhow!(
            "healthcheck actual target {target} returned {status} without the Pingora product identity"
        ))
    }
}

fn install_aws_lc_tls13_provider() -> Result<()> {
    let mut provider = rustls::crypto::aws_lc_rs::default_provider();
    provider
        .cipher_suites
        .retain(|suite| suite.version() == &rustls::version::TLS13);
    provider
        .install_default()
        .map_err(|_| anyhow!("a process-wide rustls crypto provider was installed before AWS-LC"))
}

#[inline]
fn tls_provider_name() -> &'static str {
    #[cfg(feature = "tls-aws-lc")]
    {
        "AWS-LC/rustls"
    }
    #[cfg(feature = "tls-boringssl")]
    {
        "BoringSSL"
    }
}

#[cfg(feature = "tls-boringssl")]
fn enforce_tls13(tls: &mut TlsSettings) -> Result<()> {
    use cloudflare_pingora::tls::ssl::SslVersion;

    tls.set_curves_list("X25519MLKEM768:X25519:P-256:P-384:P-521")
        .context("failed to configure BoringSSL TLS groups")?;
    tls.set_min_proto_version(Some(SslVersion::TLS1_3))
        .context("failed to set BoringSSL minimum protocol to TLS 1.3")?;
    tls.set_max_proto_version(Some(SslVersion::TLS1_3))
        .context("failed to set BoringSSL maximum protocol to TLS 1.3")?;
    Ok(())
}

#[cfg(feature = "tls-aws-lc")]
fn enforce_tls13(_tls: &mut TlsSettings) -> Result<()> {
    // The vendored rustls adapter constructs the listener with TLS 1.3 as its
    // only protocol version, and the process provider contains TLS 1.3 suites only.
    Ok(())
}

fn run(runtime: Arc<RuntimeConfig>) -> Result<()> {
    let server_config = &runtime.config.server;
    let pingora_config = ServerConf {
        threads: server_config.threads,
        upstream_keepalive_pool_size: server_config.upstream_keepalive_pool_size,
        // Pingora's value is total attempts, while the public config is retry count.
        max_retries: server_config
            .max_retries
            .checked_add(1)
            .ok_or_else(|| anyhow!("server.max_retries overflow"))?,
        grace_period_seconds: Some(0),
        graceful_shutdown_timeout_seconds: Some(server_config.graceful_shutdown_timeout_seconds),
        pid_file: "/tmp/pingora/pingora.pid".to_string(),
        upgrade_sock: "/tmp/pingora/pingora-upgrade.sock".to_string(),
        ..ServerConf::default()
    };

    let mut server = Server::new_with_opt_and_conf(None, pingora_config);
    server.bootstrap();

    let gateway = Gateway::new(runtime.clone()).context("service bootstrap failed")?;
    let mut http_options = HttpServerOptions::default();
    // Pingora 0.8.1 interprets this value as the number of HTTP/1.1 reuses
    // after the first request, while the public setting follows NGINX and
    // counts the first request. Validation guarantees this subtraction.
    http_options.keepalive_request_limit = Some(
        server_config
            .downstream_keepalive_requests
            .checked_sub(1)
            .ok_or_else(|| anyhow!("server.downstream_keepalive_requests must be positive"))?,
    );
    let mut service = ProxyServiceBuilder::new(&server.configuration, gateway)
        .name("pingora-gateway")
        .server_options(http_options)
        .build();

    let mut h2_options = default_h2_options();
    h2_options.max_concurrent_streams(server_config.http2_max_concurrent_streams);
    h2_options.max_header_list_size(16 * 1024);
    if let Some(proxy) = service.app_logic_mut() {
        proxy.h2_options = Some(h2_options);
    }

    for address in &server_config.http_listen {
        service.add_tcp_with_settings(address, listener_options(address)?);
    }
    for address in &server_config.https_listen {
        let certificate = server_config.certificate.as_deref().ok_or_else(|| {
            anyhow!(
                "TLS settings creation failed for listener {address}: certificate path is missing"
            )
        })?;
        let private_key = server_config.private_key.as_deref().ok_or_else(|| {
            anyhow!(
                "TLS settings creation failed for listener {address}: private key path is missing"
            )
        })?;
        let certificate = certificate.to_string_lossy();
        let private_key = private_key.to_string_lossy();
        let mut tls = TlsSettings::intermediate(&certificate, &private_key).with_context(|| {
            format!(
                "TLS settings creation failed for listener={address} certificate={} private_key={}",
                certificate, private_key,
            )
        })?;
        enforce_tls13(&mut tls)
            .with_context(|| format!("TLS 1.3 policy creation failed for listener={address}"))?;
        tls.enable_h2();
        service.add_tls_with_settings(address, Some(listener_options(address)?), tls);
    }

    service.add_uds(
        &server_config.health_socket.to_string_lossy(),
        Some(Permissions::from_mode(0o600)),
    );

    info!(
        "starting Pingora with {} TLS 1.3: http={:?} https={:?} health_socket={} threads={}",
        tls_provider_name(),
        server_config.http_listen,
        server_config.https_listen,
        server_config.health_socket.display(),
        server_config.threads
    );
    server.add_service(service);
    server.run_forever();
}

fn listener_options(address: &str) -> Result<TcpSocketOptions> {
    let socket = address
        .parse::<SocketAddr>()
        .with_context(|| format!("listener address parse failed: {address}"))?;
    let mut options = TcpSocketOptions::default();
    options.ipv6_only = socket.is_ipv6().then_some(true);
    options.tcp_fastopen = Some(64);
    options.tcp_keepalive = Some(TcpKeepalive {
        idle: Duration::from_secs(60),
        interval: Duration::from_secs(10),
        count: 3,
        #[cfg(target_os = "linux")]
        user_timeout: Duration::from_secs(90),
    });
    Ok(options)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ipv6_listener_explicitly_uses_v6_only() {
        assert_eq!(listener_options("[::]:443").unwrap().ipv6_only, Some(true));
        assert_eq!(listener_options("0.0.0.0:443").unwrap().ipv6_only, None);
    }
}
