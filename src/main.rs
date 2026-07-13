mod config;
mod gateway;
mod limits;
mod static_files;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use clap::Parser;
use log::info;
use pingora::apps::HttpServerOptions;
use pingora::listeners::tls::TlsSettings;
use pingora::listeners::TcpSocketOptions;
use pingora::protocols::http::v2::server::default_h2_options;
use pingora::protocols::TcpKeepalive;
use pingora::proxy::ProxyServiceBuilder;
use pingora::server::configuration::ServerConf;
use pingora::server::Server;

use crate::config::RuntimeConfig;
use crate::gateway::Gateway;

#[derive(Debug, Parser)]
#[command(version, about)]
struct Cli {
    /// Gateway configuration file.
    #[arg(
        short,
        long,
        env = "PINGOLA_CONFIG",
        default_value = "/etc/pingola/pingola.yaml"
    )]
    config: PathBuf,

    /// Validate configuration and exit without binding listeners.
    #[arg(long)]
    check: bool,
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    install_aws_lc_tls13_provider()?;

    let cli = Cli::parse();
    let runtime = Arc::new(RuntimeConfig::load(&cli.config)?);
    if cli.check {
        println!("configuration is valid: {}", cli.config.display());
        return Ok(());
    }

    run(runtime)
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

fn run(runtime: Arc<RuntimeConfig>) -> Result<()> {
    let server_config = &runtime.config.server;
    let pingora_config = ServerConf {
        threads: server_config.threads,
        upstream_keepalive_pool_size: server_config.upstream_keepalive_pool_size,
        max_retries: server_config.max_retries,
        grace_period_seconds: Some(0),
        graceful_shutdown_timeout_seconds: Some(server_config.graceful_shutdown_timeout_seconds),
        pid_file: "/tmp/pingola.pid".to_string(),
        upgrade_sock: "/tmp/pingola-upgrade.sock".to_string(),
        ..ServerConf::default()
    };

    let mut server = Server::new_with_opt_and_conf(None, pingora_config);
    server.bootstrap();

    let gateway = Gateway::new(runtime.clone())?;
    let mut http_options = HttpServerOptions::default();
    http_options.keepalive_request_limit = Some(500);
    let mut service = ProxyServiceBuilder::new(&server.configuration, gateway)
        .name("pingola-gateway")
        .server_options(http_options)
        .build();

    let mut h2_options = default_h2_options();
    h2_options.max_concurrent_streams(32);
    h2_options.max_header_list_size(16 * 1024);
    if let Some(proxy) = service.app_logic_mut() {
        proxy.h2_options = Some(h2_options);
    }

    for address in &server_config.http_listen {
        service.add_tcp_with_settings(address, listener_options());
    }
    for address in &server_config.https_listen {
        let certificate = server_config.certificate.to_string_lossy();
        let private_key = server_config.private_key.to_string_lossy();
        let mut tls = TlsSettings::intermediate(&certificate, &private_key)?;
        tls.enable_h2();
        service.add_tls_with_settings(address, Some(listener_options()), tls);
    }

    info!(
        "starting Pingola with AWS-LC TLS 1.3: http={:?} https={:?} threads={}",
        server_config.http_listen, server_config.https_listen, server_config.threads
    );
    server.add_service(service);
    server.run_forever();
}

fn listener_options() -> TcpSocketOptions {
    let mut options = TcpSocketOptions::default();
    options.tcp_fastopen = Some(64);
    options.tcp_keepalive = Some(TcpKeepalive {
        idle: Duration::from_secs(60),
        interval: Duration::from_secs(10),
        count: 3,
        #[cfg(target_os = "linux")]
        user_timeout: Duration::from_secs(90),
    });
    options
}
