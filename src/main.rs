mod config;

use std::path::PathBuf;

use anyhow::{anyhow, Result};
use clap::Parser;

use crate::config::RuntimeConfig;

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
    let runtime = RuntimeConfig::load(&cli.config)?;
    if cli.check {
        println!("configuration is valid: {}", cli.config.display());
        return Ok(());
    }

    anyhow::bail!(
        "proxy runtime is not wired yet ({} hosts loaded)",
        runtime.config.hosts.len()
    )
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
