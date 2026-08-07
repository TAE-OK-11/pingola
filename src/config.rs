use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use http::HeaderValue;
use ipnet::IpNet;
use serde::Deserialize;

fn default_threads() -> usize {
    1
}

fn default_keepalive_pool() -> usize {
    128
}

fn default_downstream_keepalive_requests() -> u32 {
    500
}

fn default_max_retries() -> usize {
    2
}

fn default_http2_max_concurrent_streams() -> u32 {
    32
}

fn default_http3_internal_listen() -> SocketAddr {
    "127.0.0.1:18080"
        .parse()
        .expect("the default HTTP/3 internal listener is valid")
}

fn default_http3_max_idle_timeout() -> u64 {
    60
}

fn default_http3_max_concurrent_streams() -> u32 {
    64
}

fn default_http3_handshake_timeout() -> u64 {
    5
}

fn default_http3_connection_rate_per_second() -> f64 {
    64.0
}

fn default_http3_connection_burst() -> u32 {
    128
}

fn default_http3_max_connections_per_ip() -> usize {
    128
}

fn default_downstream_max_connections() -> usize {
    4096
}

fn default_downstream_request_header_timeout() -> u64 {
    15
}

fn default_health_socket() -> PathBuf {
    PathBuf::from("/tmp/pingora/health.sock")
}

fn default_legacy_health_endpoint() -> bool {
    true
}

fn default_graceful_shutdown() -> u64 {
    // Container replacement should stop accepting new work immediately and only
    // spend a short bounded interval draining in-flight requests.
    5
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

fn default_upstream_http2_streams() -> usize {
    32
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
    pub http3_listen: Vec<String>,
    #[serde(default = "default_http3_internal_listen")]
    pub http3_internal_listen: SocketAddr,
    #[serde(default = "default_http3_max_idle_timeout")]
    pub http3_max_idle_timeout_seconds: u64,
    #[serde(default = "default_http3_max_concurrent_streams")]
    pub http3_max_concurrent_streams: u32,
    #[serde(default = "default_http3_handshake_timeout")]
    pub http3_handshake_timeout_seconds: u64,
    #[serde(default = "default_http3_connection_rate_per_second")]
    pub http3_connection_rate_per_second: f64,
    #[serde(default = "default_http3_connection_burst")]
    pub http3_connection_burst: u32,
    #[serde(default = "default_http3_max_connections_per_ip")]
    pub http3_max_connections_per_ip: usize,
    #[serde(default)]
    pub certificate: Option<PathBuf>,
    #[serde(default)]
    pub private_key: Option<PathBuf>,
    #[serde(default = "default_threads")]
    pub threads: usize,
    #[serde(default = "default_keepalive_pool")]
    pub upstream_keepalive_pool_size: usize,
    #[serde(default = "default_downstream_keepalive_requests")]
    pub downstream_keepalive_requests: u32,
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
    #[serde(default = "default_downstream_max_connections")]
    pub downstream_max_connections: usize,
    #[serde(default = "default_downstream_request_header_timeout")]
    pub downstream_request_header_timeout_seconds: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpstreamConfig {
    pub address: String,
    #[serde(default)]
    pub tls: bool,
    #[serde(default)]
    pub protocol: UpstreamProtocol,
    #[serde(default = "default_upstream_http2_streams")]
    pub http2_max_concurrent_streams: usize,
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

/// HTTP version policy for a single upstream.
///
/// `auto` negotiates HTTP/2 with ALPN for TLS origins and retains HTTP/1.1 for
/// plaintext origins. Plaintext HTTP/2 has no ALPN, so h2c prior knowledge must
/// be explicitly selected with `http2`.
#[derive(Debug, Clone, Copy, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum UpstreamProtocol {
    #[default]
    Auto,
    Http1,
    Http2,
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

#[derive(Clone)]
struct Http3InternalToken(HeaderValue);

impl fmt::Debug for Http3InternalToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Http3InternalToken([redacted])")
    }
}

#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    pub config: Arc<Config>,
    http3_internal_token: Option<Http3InternalToken>,
}

impl RuntimeConfig {
    pub fn load(path: &Path) -> Result<Self> {
        let data = fs::read_to_string(path)
            .with_context(|| format!("failed to read config {}", path.display()))?;
        let config: Config = serde_saphyr::from_str(&data)
            .with_context(|| format!("failed to parse config {}", path.display()))?;
        Self::new(config)
    }

    pub fn new(config: Config) -> Result<Self> {
        validate(&config)?;
        let http3_internal_token = if config.server.http3_listen.is_empty() {
            None
        } else {
            let mut token = [0_u8; 32];
            getrandom::fill(&mut token)
                .map_err(|error| anyhow!("failed to generate HTTP/3 internal token: {error}"))?;
            let token = HeaderValue::from_str(&hex::encode(token))
                .context("generated HTTP/3 internal token is not a valid header value")?;
            Some(Http3InternalToken(token))
        };

        Ok(Self {
            config: Arc::new(config),
            http3_internal_token,
        })
    }

    pub fn http3_internal_token(&self) -> Option<&HeaderValue> {
        self.http3_internal_token.as_ref().map(|token| &token.0)
    }

    pub fn http3_internal_addr(&self) -> Option<SocketAddr> {
        (!self.config.server.http3_listen.is_empty())
            .then_some(self.config.server.http3_internal_listen)
    }

    pub fn http3_public_port(&self) -> Option<u16> {
        self.config
            .server
            .http3_listen
            .first()
            .and_then(|address| address.parse::<SocketAddr>().ok())
            .map(|address| address.port())
    }

    pub fn http3_alt_svc_header(&self) -> Option<HeaderValue> {
        let port = self.http3_public_port()?;
        HeaderValue::from_str(&format!(r#"h3=":{port}"; ma=86400"#)).ok()
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

pub(crate) fn normalized_host(authority: &str) -> Cow<'_, str> {
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
    if config.server.http_listen.is_empty()
        && config.server.https_listen.is_empty()
        && config.server.http3_listen.is_empty()
    {
        bail!("at least one HTTP, HTTPS, or HTTP/3 listen address is required");
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
    if !(1..=1_000_000).contains(&config.server.downstream_keepalive_requests) {
        bail!("server.downstream_keepalive_requests must be between 1 and 1000000");
    }
    if !(1..=1024).contains(&config.server.http2_max_concurrent_streams) {
        bail!("server.http2_max_concurrent_streams must be between 1 and 1024");
    }
    if !(1..=1024).contains(&config.server.http3_max_concurrent_streams) {
        bail!("server.http3_max_concurrent_streams must be between 1 and 1024");
    }
    if !(1..=600).contains(&config.server.http3_max_idle_timeout_seconds) {
        bail!("server.http3_max_idle_timeout_seconds must be between 1 and 600");
    }
    if !(1..=30).contains(&config.server.http3_handshake_timeout_seconds) {
        bail!("server.http3_handshake_timeout_seconds must be between 1 and 30");
    }
    if !config.server.http3_connection_rate_per_second.is_finite()
        || !(0.1..=100_000.0).contains(&config.server.http3_connection_rate_per_second)
    {
        bail!("server.http3_connection_rate_per_second must be finite and between 0.1 and 100000");
    }
    if config.server.http3_connection_burst > 100_000 {
        bail!("server.http3_connection_burst must not exceed 100000");
    }
    if !(1..=1_000_000).contains(&config.server.http3_max_connections_per_ip) {
        bail!("server.http3_max_connections_per_ip must be between 1 and 1000000");
    }
    if !(1..=1_000_000).contains(&config.server.downstream_max_connections) {
        bail!("server.downstream_max_connections must be between 1 and 1000000");
    }
    if !(1..=300).contains(&config.server.downstream_request_header_timeout_seconds) {
        bail!("server.downstream_request_header_timeout_seconds must be between 1 and 300");
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
        ("HTTP/3 UDP", &config.server.http3_listen),
    ] {
        for address in addresses {
            address.parse::<SocketAddr>().with_context(|| {
                format!("server {kind} listener has invalid socket address {address}")
            })?;
        }
    }
    if (!config.server.https_listen.is_empty() || !config.server.http3_listen.is_empty())
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
        bail!("certificate and private_key are required for HTTPS or HTTP/3 listeners");
    }
    if !config.server.http3_listen.is_empty() {
        let internal = config.server.http3_internal_listen;
        if !internal.ip().is_loopback() {
            bail!("server.http3_internal_listen must use a loopback address");
        }
        let mut public_port = None;
        for address in &config.server.http3_listen {
            let address = address.parse::<SocketAddr>()?;
            if address.port() == 0 {
                bail!("server HTTP/3 listeners cannot use port zero");
            }
            match public_port {
                Some(port) if port != address.port() => {
                    bail!("all server HTTP/3 listeners must use the same UDP port")
                }
                None => public_port = Some(address.port()),
                _ => {}
            }
        }
        if config
            .server
            .http_listen
            .iter()
            .chain(&config.server.https_listen)
            .filter_map(|address| address.parse::<SocketAddr>().ok())
            .any(|address| address == internal)
        {
            bail!("server.http3_internal_listen conflicts with a public TCP listener");
        }
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

        let required_doh_upstream = match host.handler {
            HandlerKind::AdguardDns => Some("adguard_dns_doh"),
            HandlerKind::AdguardKorea => Some("adguard_korea_doh"),
            _ => None,
        };
        if let Some(required) = required_doh_upstream
            && !config.upstreams.contains_key(required)
        {
            bail!(
                "host {name} handler {:?} requires DoH upstream {required}",
                host.handler
            );
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
        if !(1..=1024).contains(&upstream.http2_max_concurrent_streams) {
            bail!("upstream {name} http2_max_concurrent_streams must be between 1 and 1024");
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
        if let Some(rate) = limit.rate_per_second
            && (!rate.is_finite() || !(0.0..=1_000_000.0).contains(&rate))
        {
            bail!("route_limits.{name}.rate_per_second must be finite and between 0 and 1000000");
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

    #[test]
    fn repository_configs_parse_with_saphyr() {
        for relative in ["config/pingora.yaml", "config/benchmark.yaml"] {
            let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(relative);
            let yaml = fs::read_to_string(&path).unwrap();
            serde_saphyr::from_str::<Config>(&yaml)
                .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        }
    }

    fn sample_config() -> Config {
        serde_saphyr::from_str(
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
    fn accepts_unique_normalized_domains() {
        let config = sample_config();
        assert_eq!(config.server.downstream_keepalive_requests, 500);
        assert!(RuntimeConfig::new(config).is_ok());
    }

    #[test]
    fn checked_in_production_config_uses_narrow_proxy_trust_and_verified_tls() {
        let config: Config =
            serde_saphyr::from_str(include_str!("../config/pingora.yaml")).unwrap();
        for broad in ["10.0.0.0/8", "172.16.0.0/12", "192.168.0.0/16"] {
            let network: IpNet = broad.parse().unwrap();
            assert!(!config.trusted_proxies.contains(&network), "{broad}");
        }
        for name in ["adguard_dns_doh", "adguard_korea_doh"] {
            let upstream = config.upstreams.get(name).unwrap();
            assert!(upstream.tls && upstream.verify_certificate, "{name}");
        }
    }

    #[test]
    fn rejects_unbounded_downstream_keepalive_requests() {
        for invalid in [0, 1_000_001] {
            let mut config = sample_config();
            config.server.downstream_keepalive_requests = invalid;
            assert!(RuntimeConfig::new(config).is_err());
        }
    }

    #[test]
    fn rejects_unbounded_downstream_admission_settings() {
        for invalid in [0, 1_000_001] {
            let mut config = sample_config();
            config.server.downstream_max_connections = invalid;
            assert!(RuntimeConfig::new(config).is_err());
        }
        for invalid in [0, 301] {
            let mut config = sample_config();
            config.server.downstream_request_header_timeout_seconds = invalid;
            assert!(RuntimeConfig::new(config).is_err());
        }
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

    #[test]
    fn parses_upstream_protocol_policy() {
        let automatic: UpstreamConfig = serde_saphyr::from_str("address: 127.0.0.1:9000").unwrap();
        assert_eq!(automatic.protocol, UpstreamProtocol::Auto);
        assert_eq!(automatic.http2_max_concurrent_streams, 32);

        let h2c: UpstreamConfig = serde_saphyr::from_str(
            "address: 127.0.0.1:9000\nprotocol: http2\nhttp2_max_concurrent_streams: 64",
        )
        .unwrap();
        assert_eq!(h2c.protocol, UpstreamProtocol::Http2);
        assert_eq!(h2c.http2_max_concurrent_streams, 64);
    }

    #[test]
    fn rejects_unbounded_upstream_http2_streams() {
        let mut config = sample_config();
        config
            .upstreams
            .get_mut("app")
            .unwrap()
            .http2_max_concurrent_streams = 0;
        assert!(RuntimeConfig::new(config).is_err());
    }

    #[test]
    fn adguard_handler_requires_its_internal_doh_upstream_during_validation() {
        let mut config = sample_config();
        config.hosts.get_mut("app").unwrap().handler = HandlerKind::AdguardDns;
        let error = RuntimeConfig::new(config.clone()).unwrap_err();
        assert!(format!("{error:#}").contains("adguard_dns_doh"));

        let upstream = config.upstreams.get("app").unwrap().clone();
        config.upstreams.insert("adguard_dns_doh".into(), upstream);
        assert!(RuntimeConfig::new(config).is_ok());
    }
}
