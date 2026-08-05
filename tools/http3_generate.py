#!/usr/bin/env python3
from pathlib import Path


def replace(path: str, old: str, new: str, count: int = 1) -> None:
    file = Path(path)
    text = file.read_text()
    actual = text.count(old)
    if actual != count:
        raise SystemExit(f"{path}: expected {count} matches, found {actual}: {old!r}")
    file.write_text(text.replace(old, new))


# Configuration schema and validation.
replace(
    "src/config.rs",
    "use anyhow::{Context, Result, bail};\nuse ipnet::IpNet;",
    "use anyhow::{Context, Result, bail};\nuse http::HeaderValue;\nuse ipnet::IpNet;",
)
replace(
    "src/config.rs",
    "fn default_http2_max_concurrent_streams() -> u32 {\n    32\n}\n",
    '''fn default_http2_max_concurrent_streams() -> u32 {
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
''',
)
replace(
    "src/config.rs",
    '''    pub https_listen: Vec<String>,
    #[serde(default)]
    pub certificate: Option<PathBuf>,''',
    '''    pub https_listen: Vec<String>,
    #[serde(default)]
    pub http3_listen: Vec<String>,
    #[serde(default = "default_http3_internal_listen")]
    pub http3_internal_listen: SocketAddr,
    #[serde(default = "default_http3_max_idle_timeout")]
    pub http3_max_idle_timeout_seconds: u64,
    #[serde(default = "default_http3_max_concurrent_streams")]
    pub http3_max_concurrent_streams: u32,
    #[serde(default)]
    pub certificate: Option<PathBuf>,''',
)
replace(
    "src/config.rs",
    "    pub fn is_trusted_proxy(&self, ip: std::net::IpAddr) -> bool {",
    '''    pub fn http3_internal_addr(&self) -> Option<SocketAddr> {
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
        HeaderValue::from_str(&format!(r#"h3=\":{port}\"; ma=86400"#)).ok()
    }

    pub fn is_trusted_proxy(&self, ip: std::net::IpAddr) -> bool {''',
)
replace(
    "src/config.rs",
    '''    if config.server.http_listen.is_empty() && config.server.https_listen.is_empty() {
        bail!("at least one HTTP or HTTPS listen address is required");
    }''',
    '''    if config.server.http_listen.is_empty()
        && config.server.https_listen.is_empty()
        && config.server.http3_listen.is_empty()
    {
        bail!("at least one HTTP, HTTPS, or HTTP/3 listen address is required");
    }''',
)
replace(
    "src/config.rs",
    '''    if !(1..=1024).contains(&config.server.http2_max_concurrent_streams) {
        bail!("server.http2_max_concurrent_streams must be between 1 and 1024");
    }''',
    '''    if !(1..=1024).contains(&config.server.http2_max_concurrent_streams) {
        bail!("server.http2_max_concurrent_streams must be between 1 and 1024");
    }
    if !(1..=1024).contains(&config.server.http3_max_concurrent_streams) {
        bail!("server.http3_max_concurrent_streams must be between 1 and 1024");
    }
    if !(1..=600).contains(&config.server.http3_max_idle_timeout_seconds) {
        bail!("server.http3_max_idle_timeout_seconds must be between 1 and 600");
    }''',
)
replace(
    "src/config.rs",
    '''    for (kind, addresses) in [
        ("HTTP", &config.server.http_listen),
        ("HTTPS", &config.server.https_listen),
    ] {''',
    '''    for (kind, addresses) in [
        ("HTTP", &config.server.http_listen),
        ("HTTPS", &config.server.https_listen),
        ("HTTP/3 UDP", &config.server.http3_listen),
    ] {''',
)
replace(
    "src/config.rs",
    '''    if !config.server.https_listen.is_empty()
        && (config''',
    '''    if (!config.server.https_listen.is_empty() || !config.server.http3_listen.is_empty())
        && (config''',
)
replace(
    "src/config.rs",
    '''        bail!("certificate and private_key are required for HTTPS listeners");
    }''',
    '''        bail!("certificate and private_key are required for HTTPS or HTTP/3 listeners");
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
    }''',
)

# Pingora server wiring.
replace("src/main.rs", "mod gateway;\n", "mod gateway;\nmod http3;\n")
replace(
    "src/main.rs",
    '''            runtime.config.server.http_listen.len() + runtime.config.server.https_listen.len()
''',
    '''            runtime.config.server.http_listen.len()
                + runtime.config.server.https_listen.len()
                + runtime.config.server.http3_listen.len()
                + usize::from(!runtime.config.server.http3_listen.is_empty())
''',
)
replace(
    "src/main.rs",
    "    for address in &server_config.https_listen {",
    '''    let http3_internal = server_config.http3_internal_listen.to_string();
    if !server_config.http3_listen.is_empty() {
        service.add_tcp_with_settings(
            &http3_internal,
            listener_options(&http3_internal)?,
        );
    }
    for address in &server_config.https_listen {''',
)
replace(
    "src/main.rs",
    '''    info!(
        "starting Pingora with {} TLS 1.3: http={:?} https={:?} health_socket={} threads={}",''',
    '''    http3::start(runtime.clone()).context("HTTP/3 frontend startup failed")?;

    info!(
        "starting Pingora with {} TLS 1.3: http={:?} https={:?} http3_udp={:?} http3_internal={} health_socket={} threads={}",''',
)
replace(
    "src/main.rs",
    '''        server_config.https_listen,
        server_config.health_socket.display(),''',
    '''        server_config.https_listen,
        server_config.http3_listen,
        server_config.http3_internal_listen,
        server_config.health_socket.display(),''',
)

# Trusted loopback handoff and controlled Alt-Svc.
replace(
    "src/gateway.rs",
    'const X_FORWARDED_FOR: HeaderName = HeaderName::from_static("x-forwarded-for");',
    '''const X_FORWARDED_FOR: HeaderName = HeaderName::from_static("x-forwarded-for");
const HTTP3_INTERNAL: HeaderName = HeaderName::from_static("x-jbs-http3-internal");
const HTTP3_PORT: HeaderName = HeaderName::from_static("x-jbs-http3-port");''',
)
replace(
    "src/gateway.rs",
    '''    tls: bool,
    body_bytes: usize,''',
    '''    tls: bool,
    http3: bool,
    forwarded_port: Option<u16>,
    body_bytes: usize,''',
)
replace(
    "src/gateway.rs",
    '''            tls: false,
            body_bytes: 0,''',
    '''            tls: false,
            http3: false,
            forwarded_port: None,
            body_bytes: 0,''',
)
replace(
    "src/gateway.rs",
    '''        let tls = is_tls(session);
        let path = session.req_header().uri.path();''',
    '''        let http3 = is_internal_http3(&self.runtime, session);
        let tls = is_tls(session) || http3;
        ctx.http3 = http3;
        ctx.forwarded_port = http3
            .then(|| {
                session
                    .req_header()
                    .headers
                    .get(&HTTP3_PORT)
                    .and_then(|value| value.to_str().ok())
                    .and_then(|value| value.parse::<u16>().ok())
                    .or_else(|| self.runtime.http3_public_port())
            })
            .flatten();
        let path = session.req_header().uri.path();''',
)
replace(
    "src/gateway.rs",
    '''        upstream_request.remove_header(&FORWARDED);
        upstream_request.remove_header(&X_FORWARDED_FOR);''',
    '''        upstream_request.remove_header(&FORWARDED);
        upstream_request.remove_header(&X_FORWARDED_FOR);
        upstream_request.remove_header(&HTTP3_INTERNAL);
        upstream_request.remove_header(&HTTP3_PORT);''',
)
replace(
    "src/gateway.rs",
    '''        let listener_port = session
            .server_addr()
            .and_then(|address| address.as_inet())
            .map(|address| address.port());''',
    '''        let listener_port = ctx.forwarded_port.or_else(|| {
            session
                .server_addr()
                .and_then(|address| address.as_inet())
                .map(|address| address.port())
        });''',
)
replace(
    "src/gateway.rs",
    "        insert_security_headers(response, plan.handler, ctx.tls)?;",
    '''        insert_security_headers(response, plan.handler, ctx.tls)?;
        if ctx.tls
            && let Some(alt_svc) = self.runtime.http3_alt_svc_header()
        {
            response.insert_header("alt-svc", alt_svc)?;
        }''',
)
replace(
    "src/gateway.rs",
    '''fn is_tls(session: &Session) -> bool {
    session
        .digest()
        .and_then(|digest| digest.ssl_digest.as_ref())
        .is_some()
}
''',
    '''fn is_tls(session: &Session) -> bool {
    session
        .digest()
        .and_then(|digest| digest.ssl_digest.as_ref())
        .is_some()
}

fn is_internal_http3(runtime: &RuntimeConfig, session: &Session) -> bool {
    let Some(expected) = runtime.http3_internal_addr() else {
        return false;
    };
    let server_matches = session
        .server_addr()
        .and_then(|address| address.as_inet())
        .is_some_and(|address| address == expected);
    let peer_is_loopback = session
        .client_addr()
        .and_then(|address| address.as_inet())
        .is_some_and(|address| address.ip().is_loopback());
    let marker_matches = session
        .req_header()
        .headers
        .get(&HTTP3_INTERNAL)
        .is_some_and(|value| value == "1");
    server_matches && peer_is_loopback && marker_matches
}
''',
)

# Preflight checks both UDP and the loopback handoff listener.
replace(
    "src/preflight.rs",
    "    if server.https_listen.is_empty() {",
    "    if server.https_listen.is_empty() && server.http3_listen.is_empty() {",
)
replace(
    "src/preflight.rs",
    "        let count = server.http_listen.len() + server.https_listen.len();",
    '''        let count = server.http_listen.len()
            + server.https_listen.len()
            + server.http3_listen.len()
            + usize::from(!server.http3_listen.is_empty());''',
)
replace(
    "src/preflight.rs",
    '''    drop(sockets);
}

fn bind_listener(address: &str) -> Result<Socket> {''',
    '''    if !runtime.config.server.http3_listen.is_empty() {
        let address = runtime.config.server.http3_internal_listen;
        match bind_listener(&address.to_string()) {
            Ok(socket) => {
                report.ok(
                    format!("listener bind HTTP/3 internal {address}"),
                    "bound loopback TCP",
                );
                sockets.push(socket);
            }
            Err(error) => report.error(
                format!("listener bind HTTP/3 internal {address}"),
                format!("failed to bind internal TCP address {address}: {error:#}"),
            ),
        }
    }
    let mut udp_sockets = Vec::new();
    for address in &runtime.config.server.http3_listen {
        match bind_udp_listener(address) {
            Ok(socket) => {
                report.ok(
                    format!("listener bind HTTP/3 UDP {address}"),
                    if address.starts_with('[') {
                        "bound with IPV6_V6ONLY=true".to_string()
                    } else {
                        "bound".to_string()
                    },
                );
                udp_sockets.push(socket);
            }
            Err(error) => report.error(
                format!("listener bind HTTP/3 UDP {address}"),
                format!("failed to bind UDP address {address}: {error:#}"),
            ),
        }
    }
    drop(udp_sockets);
    drop(sockets);
}

fn bind_udp_listener(address: &str) -> Result<Socket> {
    let address = address
        .parse::<SocketAddr>()
        .with_context(|| format!("invalid UDP listener address {address}"))?;
    let domain = if address.is_ipv6() {
        Domain::IPV6
    } else {
        Domain::IPV4
    };
    let socket = Socket::new(domain, Type::DGRAM, Some(Protocol::UDP))
        .with_context(|| format!("failed to create UDP socket for {address}"))?;
    socket
        .set_reuse_address(true)
        .with_context(|| format!("failed to set SO_REUSEADDR for UDP {address}"))?;
    if address.is_ipv6() {
        socket
            .set_only_v6(true)
            .with_context(|| format!("failed to set IPV6_V6ONLY for UDP {address}"))?;
    }
    socket
        .bind(&address.into())
        .with_context(|| format!("UDP bind failed for {address}"))?;
    Ok(socket)
}

fn bind_listener(address: &str) -> Result<Socket> {''',
)

# Production defaults: TCP and UDP share port 443, with a loopback-only bridge.
replace(
    "config/pingora.yaml",
    '''  https_listen:
    - "0.0.0.0:443"
    - "[::]:443"
''',
    '''  https_listen:
    - "0.0.0.0:443"
    - "[::]:443"
  http3_listen:
    - "0.0.0.0:443"
    - "[::]:443"
  http3_internal_listen: "127.0.0.1:18080"
  http3_max_idle_timeout_seconds: 60
  http3_max_concurrent_streams: 64
''',
)
