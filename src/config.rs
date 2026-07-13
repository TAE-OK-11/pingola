use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::net::SocketAddr;
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

fn default_http2_max_concurrent_streams() -> u32 {
    32
}

fn default_health_socket() -> PathBuf {
    PathBuf::from("/tmp/pingora/health.sock")
}

fn default_legacy_health_endpoint() -> bool {
    true
}

fn default_graceful_shutdown() -> u64 {
    60
}

fn default_body_limit() -> usize {
    100 * 1024 * 1024
}

fn default_static_cache() -> usize {
    32 * 1024 * 1024
}

fn default_true() -> bool {
    true
}

fn default_connect_timeout() -> u64 {
    5
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
    #[serde(default)]
    pub route_limits: BTreeMap<String, RouteLimitConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    pub http_listen: Vec<String>,
    pub https_listen: Vec<String>,
    #[serde(default)]
    pub certificate: Option<PathBuf>,
    #[serde(default)]
    pub private_key: Option<PathBuf>,
    #[serde(default = "default_threads")]
    pub threads: usize,
    #[serde(default = "default_keepalive_pool")]
    pub upstream_keepalive_pool_size: usize,
    #[serde(default = "default_max_retries")]
    pub max_retries: usize,
    #[serde(default = "default_graceful_shutdown")]
    pub graceful_shutdown_timeout_seconds: u64,
    #[serde(default = "default_static_cache")]
    pub static_cache_bytes: usize,
    #[serde(default)]
    pub access_log: bool,
    #[serde(default = "default_health_socket")]
    pub health_socket: PathBuf,
    #[serde(default = "default_legacy_health_endpoint")]
    pub legacy_pingola_health: bool,
    #[serde(default)]
    pub health_details: bool,
    #[serde(default)]
    pub global_active_requests: usize,
    #[serde(default = "default_http2_max_concurrent_streams")]
    pub http2_max_concurrent_streams: u32,
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
    #[serde(default)]
    pub read_timeout_seconds: Option<u64>,
    #[serde(default)]
    pub write_timeout_seconds: Option<u64>,
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

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteLimitConfig {
    /// Requests per second. Zero disables the rate limiter for this route.
    #[serde(default)]
    pub rate_per_second: Option<f64>,
    /// Extra token-bucket capacity. Zero means no burst beyond the base rate.
    #[serde(default)]
    pub burst: Option<u32>,
    /// Concurrent active requests/H2 streams. Zero disables this route limit.
    #[serde(default)]
    pub active_requests: Option<usize>,
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

    pub fn host(&self, authority: &str) -> Option<(&str, &str, &HostConfig)> {
        let domain = normalized_host(authority);
        let (canonical_domain, name) = self.hosts_by_domain.get_key_value(domain.as_ref())?;
        Some((
            canonical_domain.as_str(),
            name.as_str(),
            self.config.hosts.get(name)?,
        ))
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
    normalized_host(authority).into_owned()
}

fn normalized_host(authority: &str) -> Cow<'_, str> {
    let authority = authority.trim().trim_end_matches('.');
    let host = if let Some(stripped) = authority.strip_prefix('[') {
        stripped.split_once(']').map_or(authority, |(host, _)| host)
    } else {
        match authority.rsplit_once(':') {
            Some((host, port)) if port.parse::<u16>().is_ok() => host,
            _ => authority,
        }
    };
    if host.bytes().any(|byte| byte.is_ascii_uppercase()) {
        Cow::Owned(host.to_ascii_lowercase())
    } else {
        Cow::Borrowed(host)
    }
}

fn validate(config: &Config) -> Result<()> {
    if config.server.http_listen.is_empty() && config.server.https_listen.is_empty() {
        bail!("at least one HTTP or HTTPS listen address is required");
    }
    if config.server.threads == 0 {
        bail!("server.threads must be greater than zero");
    }
    if config.server.threads > 64 {
        bail!("server.threads must not exceed 64");
    }
    if config.server.max_retries > 10 {
        bail!("server.max_retries must not exceed 10");
    }
    if !(1..=1024).contains(&config.server.http2_max_concurrent_streams) {
        bail!("server.http2_max_concurrent_streams must be between 1 and 1024");
    }
    if config.server.static_cache_bytes == 0 {
        bail!("server.static_cache_bytes must be greater than zero");
    }
    if config.server.health_socket.as_os_str().is_empty()
        || !config.server.health_socket.is_absolute()
    {
        bail!("server.health_socket must be an absolute path");
    }
    for (kind, addresses) in [
        ("HTTP", &config.server.http_listen),
        ("HTTPS", &config.server.https_listen),
    ] {
        for address in addresses {
            address.parse::<SocketAddr>().with_context(|| {
                format!("server {kind} listener has invalid socket address {address}")
            })?;
        }
    }
    if !config.server.https_listen.is_empty()
        && (config
            .server
            .certificate
            .as_ref()
            .is_none_or(|path| path.as_os_str().is_empty())
            || config
                .server
                .private_key
                .as_ref()
                .is_none_or(|path| path.as_os_str().is_empty()))
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
        if upstream.connect_timeout_seconds == 0 || upstream.idle_timeout_seconds == 0 {
            bail!("upstream {name} timeout values must be greater than zero");
        }
        if upstream.read_timeout_seconds == Some(0) || upstream.write_timeout_seconds == Some(0) {
            bail!("upstream {name} explicit read/write timeouts must be greater than zero");
        }
    }

    const ROUTES: &[&str] = &[
        "navidrome_stream",
        "navidrome_cover",
        "navidrome_api",
        "vaultwarden_auth",
        "vaultwarden_hub",
        "vaultwarden",
        "couchdb",
        "doh",
        "adguard_ui",
    ];
    for (name, limit) in &config.route_limits {
        if !ROUTES.contains(&name.as_str()) {
            bail!("route_limits contains unknown route {name}");
        }
        if let Some(rate) = limit.rate_per_second {
            if !rate.is_finite() || !(0.0..=1_000_000.0).contains(&rate) {
                bail!(
                    "route_limits.{name}.rate_per_second must be finite and between 0 and 1000000"
                );
            }
        }
        if limit.burst.is_some_and(|burst| burst > 1_000_000) {
            bail!("route_limits.{name}.burst must not exceed 1000000");
        }
        if limit
            .active_requests
            .is_some_and(|active| active > 1_000_000)
        {
            bail!("route_limits.{name}.active_requests must not exceed 1000000");
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
        let (domain, name, _) = runtime.host("APP.EXAMPLE.COM:443").unwrap();
        assert_eq!(domain, "app.example.com");
        assert_eq!(name, "app");
    }

    #[test]
    fn rejects_unknown_upstream() {
        let mut config = sample_config();
        config.hosts.get_mut("app").unwrap().upstream = Some("missing".into());
        assert!(RuntimeConfig::new(config).is_err());
    }

    #[test]
    fn rejects_invalid_listener_address() {
        let mut config = sample_config();
        config.server.http_listen = vec!["localhost:not-a-port".into()];
        assert!(RuntimeConfig::new(config).is_err());
    }

    #[test]
    fn accepts_http_only_without_tls_files() {
        assert!(RuntimeConfig::new(sample_config()).is_ok());
    }

    #[test]
    fn rejects_non_finite_route_rate() {
        let mut config = sample_config();
        config.route_limits.insert(
            "doh".into(),
            RouteLimitConfig {
                rate_per_second: Some(f64::INFINITY),
                ..RouteLimitConfig::default()
            },
        );
        assert!(RuntimeConfig::new(config).is_err());
    }
}
