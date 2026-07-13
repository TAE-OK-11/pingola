use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use ipnet::IpNet;
use serde::Deserialize;

fn default_threads() -> usize {
    1
}

fn default_keepalive_pool() -> usize {
    128
}

fn default_max_retries() -> usize {
    2
}

fn default_graceful_shutdown() -> u64 {
    60
}

fn default_body_limit() -> usize {
    100 * 1024 * 1024
}

fn default_true() -> bool {
    true
}

fn default_connect_timeout() -> u64 {
    5
}

fn default_read_timeout() -> u64 {
    60
}

fn default_idle_timeout() -> u64 {
    15
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub server: ServerConfig,
    pub trusted_proxies: Vec<IpNet>,
    pub upstreams: BTreeMap<String, UpstreamConfig>,
    pub hosts: BTreeMap<String, HostConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    pub http_listen: Vec<String>,
    pub https_listen: Vec<String>,
    pub certificate: PathBuf,
    pub private_key: PathBuf,
    #[serde(default = "default_threads")]
    pub threads: usize,
    #[serde(default = "default_keepalive_pool")]
    pub upstream_keepalive_pool_size: usize,
    #[serde(default = "default_max_retries")]
    pub max_retries: usize,
    #[serde(default = "default_graceful_shutdown")]
    pub graceful_shutdown_timeout_seconds: u64,
    #[serde(default)]
    pub access_log: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpstreamConfig {
    pub address: String,
    #[serde(default)]
    pub tls: bool,
    #[serde(default)]
    pub sni: Option<String>,
    #[serde(default = "default_true")]
    pub verify_certificate: bool,
    #[serde(default = "default_connect_timeout")]
    pub connect_timeout_seconds: u64,
    #[serde(default = "default_read_timeout")]
    pub read_timeout_seconds: u64,
    #[serde(default = "default_read_timeout")]
    pub write_timeout_seconds: u64,
    #[serde(default = "default_idle_timeout")]
    pub idle_timeout_seconds: u64,
}

#[derive(Debug, Clone, Copy, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum HandlerKind {
    Static,
    NavidromeMain,
    NavidromeCdn,
    Vaultwarden,
    Couchdb,
    AdguardDns,
    AdguardKorea,
}

impl HandlerKind {
    pub fn is_static(self) -> bool {
        self == Self::Static
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostConfig {
    pub domains: Vec<String>,
    pub handler: HandlerKind,
    #[serde(default)]
    pub upstream: Option<String>,
    #[serde(default)]
    pub static_root: Option<PathBuf>,
    #[serde(default)]
    pub redirect_http: bool,
    #[serde(default = "default_body_limit")]
    pub max_body_bytes: usize,
}

#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    pub config: Arc<Config>,
    hosts_by_domain: HashMap<String, String>,
}

impl RuntimeConfig {
    pub fn load(path: &Path) -> Result<Self> {
        let data = fs::read_to_string(path)
            .with_context(|| format!("failed to read config {}", path.display()))?;
        let config: Config = serde_yaml::from_str(&data)
            .with_context(|| format!("failed to parse config {}", path.display()))?;
        Self::new(config)
    }

    pub fn new(config: Config) -> Result<Self> {
        validate(&config)?;

        let mut hosts_by_domain = HashMap::new();
        for (name, host) in &config.hosts {
            for domain in &host.domains {
                hosts_by_domain.insert(domain.to_ascii_lowercase(), name.clone());
            }
        }

        Ok(Self {
            config: Arc::new(config),
            hosts_by_domain,
        })
    }

    pub fn host(&self, authority: &str) -> Option<(&str, &HostConfig)> {
        let domain = normalize_host(authority);
        let name = self.hosts_by_domain.get(&domain)?;
        Some((name.as_str(), self.config.hosts.get(name)?))
    }

    pub fn upstream(&self, name: &str) -> Option<&UpstreamConfig> {
        self.config.upstreams.get(name)
    }

    pub fn is_trusted_proxy(&self, ip: std::net::IpAddr) -> bool {
        self.config
            .trusted_proxies
            .iter()
            .any(|network| network.contains(&ip))
    }
}

pub fn normalize_host(authority: &str) -> String {
    let authority = authority.trim().trim_end_matches('.').to_ascii_lowercase();
    if let Some(stripped) = authority.strip_prefix('[') {
        return stripped
            .split_once(']')
            .map_or_else(|| authority.clone(), |(host, _)| host.to_string());
    }

    match authority.rsplit_once(':') {
        Some((host, port)) if port.parse::<u16>().is_ok() => host.to_string(),
        _ => authority,
    }
}

fn validate(config: &Config) -> Result<()> {
    if config.server.http_listen.is_empty() && config.server.https_listen.is_empty() {
        bail!("at least one HTTP or HTTPS listen address is required");
    }
    if config.server.threads == 0 {
        bail!("server.threads must be greater than zero");
    }
    if !config.server.https_listen.is_empty()
        && (config.server.certificate.as_os_str().is_empty()
            || config.server.private_key.as_os_str().is_empty())
    {
        bail!("certificate and private_key are required for HTTPS listeners");
    }
    if config.hosts.is_empty() {
        bail!("at least one host is required");
    }

    let mut seen = HashMap::<String, String>::new();
    for (name, host) in &config.hosts {
        if host.domains.is_empty() {
            bail!("host {name} has no domains");
        }
        if host.max_body_bytes == 0 {
            bail!("host {name} max_body_bytes must be greater than zero");
        }

        if host.handler.is_static() {
            if host.static_root.is_none() {
                bail!("static host {name} requires static_root");
            }
            if host.upstream.is_some() {
                bail!("static host {name} cannot define upstream");
            }
        } else {
            let upstream = host
                .upstream
                .as_deref()
                .with_context(|| format!("proxy host {name} requires upstream"))?;
            if !config.upstreams.contains_key(upstream) {
                bail!("host {name} references missing upstream {upstream}");
            }
        }

        for domain in &host.domains {
            let normalized = normalize_host(domain);
            if normalized.is_empty() || normalized != domain.to_ascii_lowercase() {
                bail!("host {name} contains invalid canonical domain {domain}");
            }
            if let Some(previous) = seen.insert(normalized.clone(), name.clone()) {
                bail!("domain {normalized} is declared by both {previous} and {name}");
            }
        }
    }

    for (name, upstream) in &config.upstreams {
        if upstream.address.is_empty() {
            bail!("upstream {name} has an empty address");
        }
        if upstream.tls && upstream.sni.as_deref().unwrap_or_default().is_empty() {
            bail!("TLS upstream {name} requires sni");
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_config() -> Config {
        serde_yaml::from_str(
            r#"
server:
  http_listen: ["127.0.0.1:8080"]
  https_listen: []
  certificate: /tmp/cert.pem
  private_key: /tmp/key.pem
trusted_proxies: ["127.0.0.0/8"]
upstreams:
  app:
    address: "127.0.0.1:9000"
hosts:
  app:
    domains: ["app.example.com"]
    handler: navidrome-main
    upstream: app
"#,
        )
        .unwrap()
    }

    #[test]
    fn normalizes_authority() {
        assert_eq!(normalize_host("Music.Example.COM:443"), "music.example.com");
        assert_eq!(normalize_host("example.com."), "example.com");
        assert_eq!(normalize_host("[::1]:443"), "::1");
    }

    #[test]
    fn resolves_host_case_insensitively() {
        let runtime = RuntimeConfig::new(sample_config()).unwrap();
        let (name, _) = runtime.host("APP.EXAMPLE.COM:443").unwrap();
        assert_eq!(name, "app");
    }

    #[test]
    fn rejects_unknown_upstream() {
        let mut config = sample_config();
        config.hosts.get_mut("app").unwrap().upstream = Some("missing".into());
        assert!(RuntimeConfig::new(config).is_err());
    }
}
