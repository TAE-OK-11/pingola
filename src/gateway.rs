use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use bytes::Bytes;
use http::header::{
    ACCEPT_ENCODING, CACHE_CONTROL, CONNECTION, CONTENT_LENGTH, EXPIRES, FORWARDED, HOST,
    LAST_MODIFIED, PRAGMA, UPGRADE,
};
use log::{info, warn};
use pingora::http::{RequestHeader, ResponseHeader};
use pingora::prelude::HttpPeer;
use pingora::protocols::{TcpKeepalive, ALPN};
use pingora::proxy::{ProxyHttp, Session};
use pingora::Error;
use pingora::ErrorType::HTTPStatus;
use pingora::Result;

use crate::config::{normalize_host, HandlerKind, HostConfig, RuntimeConfig};
use crate::limits::{ConnectionLimiter, ConnectionPermit, RateLimiter};
use crate::static_files::StaticFiles;

const STREAM_PREFIXES: &[&str] = &[
    "/rest/stream",
    "/rest/download",
    "/stream",
    "/play",
    "/ext/stream",
];
const COVER_PREFIXES: &[&str] = &["/rest/getCoverArt", "/api/artwork", "/coverart", "/artwork"];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RouteClass {
    NavidromeStream,
    NavidromeCover,
    NavidromeApi,
    VaultwardenAuth,
    VaultwardenHub,
    Vaultwarden,
    Couchdb,
    Doh,
    AdguardUi,
}

impl RouteClass {
    fn rate_limit(self) -> Option<(&'static str, f64, u32)> {
        match self {
            Self::NavidromeStream => Some(("navidrome_stream", 40.0, 15)),
            Self::NavidromeCover => Some(("navidrome_api", 20.0, 20)),
            Self::NavidromeApi => Some(("navidrome_api", 20.0, 30)),
            Self::VaultwardenAuth => Some(("auth", 5.0 / 60.0, 3)),
            Self::Doh => Some(("doh", 100.0, 200)),
            _ => None,
        }
    }

    fn timeout_seconds(self) -> u64 {
        match self {
            Self::NavidromeStream | Self::Couchdb => 3600,
            Self::VaultwardenHub => 86_400,
            Self::Vaultwarden | Self::AdguardUi => 300,
            Self::Doh => 30,
            _ => 60,
        }
    }
}

#[derive(Clone)]
struct RequestPlan {
    domain: String,
    handler: HandlerKind,
    upstream_name: String,
    route: RouteClass,
    max_body_bytes: usize,
    tls: bool,
}

pub struct RequestContext {
    plan: Option<RequestPlan>,
    client_ip: IpAddr,
    body_bytes: usize,
    retries: usize,
    started_at: Instant,
    _connection_permit: Option<ConnectionPermit>,
}

impl Default for RequestContext {
    fn default() -> Self {
        Self {
            plan: None,
            client_ip: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            body_bytes: 0,
            retries: 0,
            started_at: Instant::now(),
            _connection_permit: None,
        }
    }
}

pub struct Gateway {
    runtime: Arc<RuntimeConfig>,
    static_files: StaticFiles,
    rates: RateLimiter,
    connections: ConnectionLimiter,
}

impl Gateway {
    pub fn new(runtime: Arc<RuntimeConfig>) -> anyhow::Result<Self> {
        let roots = runtime
            .config
            .hosts
            .iter()
            .filter_map(|(name, host)| {
                host.static_root
                    .as_ref()
                    .map(|root| (name.clone(), root.clone()))
            })
            .collect::<HashMap<_, _>>();
        let static_files = StaticFiles::new(roots, runtime.config.server.static_cache_bytes)?;
        Ok(Self {
            runtime,
            static_files,
            rates: RateLimiter::new(),
            connections: ConnectionLimiter::new(),
        })
    }

    fn request_plan(
        &self,
        domain: String,
        host: &HostConfig,
        path: &str,
        tls: bool,
    ) -> Option<RequestPlan> {
        let (route, upstream_name) = match host.handler {
            HandlerKind::Static => return None,
            HandlerKind::NavidromeMain | HandlerKind::NavidromeCdn => {
                let route = if STREAM_PREFIXES
                    .iter()
                    .any(|prefix| path.starts_with(prefix))
                {
                    RouteClass::NavidromeStream
                } else if COVER_PREFIXES.iter().any(|prefix| path.starts_with(prefix)) {
                    RouteClass::NavidromeCover
                } else {
                    RouteClass::NavidromeApi
                };
                (route, host.upstream.clone()?)
            }
            HandlerKind::Vaultwarden => {
                let route = if vaultwarden_auth_path(path) {
                    RouteClass::VaultwardenAuth
                } else if path.starts_with("/notifications/hub") {
                    RouteClass::VaultwardenHub
                } else {
                    RouteClass::Vaultwarden
                };
                (route, host.upstream.clone()?)
            }
            HandlerKind::Couchdb => (RouteClass::Couchdb, host.upstream.clone()?),
            HandlerKind::AdguardDns | HandlerKind::AdguardKorea => {
                if path.starts_with("/dns-query") {
                    let upstream = match host.handler {
                        HandlerKind::AdguardDns => "adguard_dns_doh",
                        HandlerKind::AdguardKorea => "adguard_korea_doh",
                        _ => unreachable!(),
                    };
                    (RouteClass::Doh, upstream.to_string())
                } else {
                    (RouteClass::AdguardUi, host.upstream.clone()?)
                }
            }
        };

        Some(RequestPlan {
            domain,
            handler: host.handler,
            upstream_name,
            route,
            max_body_bytes: host.max_body_bytes,
            tls,
        })
    }
}

#[async_trait]
impl ProxyHttp for Gateway {
    type CTX = RequestContext;

    fn new_ctx(&self) -> Self::CTX {
        RequestContext::default()
    }

    async fn request_filter(&self, session: &mut Session, ctx: &mut Self::CTX) -> Result<bool> {
        let tls = is_tls(session);
        let path = session.req_header().uri.path().to_string();

        if path == "/nginx-health" || path == "/pingola-health" {
            return send_empty(session, 204, None, tls, &[]).await;
        }

        let Some(authority) = request_authority(session.req_header()) else {
            return send_empty(session, 400, None, tls, &[]).await;
        };
        let domain = normalize_host(&authority);
        let Some((host_name, host)) = self.runtime.host(&authority) else {
            session.set_keepalive(None);
            return send_empty(session, 421, None, tls, &[]).await;
        };
        let host_name = host_name.to_string();
        let host = host.clone();

        if !tls && host.redirect_http {
            let path_and_query = session
                .req_header()
                .uri
                .path_and_query()
                .map_or("/", |value| value.as_str());
            let location = format!("https://{domain}{path_and_query}");
            return send_empty(
                session,
                308,
                Some(host.handler),
                false,
                &[("location", location.as_str())],
            )
            .await;
        }

        if host.handler == HandlerKind::Static {
            return self.static_files.serve(&host_name, session, tls).await;
        }

        if host.handler == HandlerKind::NavidromeMain && path == "/" {
            let location = format!("https://{domain}/app/");
            return send_empty(
                session,
                308,
                Some(host.handler),
                tls,
                &[("location", location.as_str())],
            )
            .await;
        }

        let peer_ip = session
            .client_addr()
            .and_then(|address| address.as_inet())
            .map_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED), |address| address.ip());
        let forwarded_for = session
            .req_header()
            .headers
            .get("x-forwarded-for")
            .and_then(|value| value.to_str().ok());
        let client_ip = resolve_client_ip(&self.runtime, peer_ip, forwarded_for);
        let Some(plan) = self.request_plan(domain, &host, &path, tls) else {
            return send_empty(session, 500, Some(host.handler), tls, &[]).await;
        };

        if content_length(session.req_header()).is_some_and(|length| length > plan.max_body_bytes) {
            return send_empty(session, 413, Some(plan.handler), tls, &[]).await;
        }

        if let Some((zone, rate, burst)) = plan.route.rate_limit() {
            if !self.rates.allow(zone, client_ip, rate, burst) {
                return send_empty(
                    session,
                    429,
                    Some(plan.handler),
                    tls,
                    &[("retry-after", "1")],
                )
                .await;
            }
        }

        let connection_limit = connection_limit(plan.handler, plan.route);
        let Some(permit) = self
            .connections
            .acquire("conn_per_ip", client_ip, connection_limit)
        else {
            return send_empty(
                session,
                429,
                Some(plan.handler),
                tls,
                &[("retry-after", "1")],
            )
            .await;
        };

        let timeout = Duration::from_secs(plan.route.timeout_seconds());
        session.set_read_timeout(Some(timeout));
        session.set_write_timeout(Some(timeout));
        session.set_keepalive(Some(30));
        session.set_keepalive_reuses_remaining(Some(500));

        ctx.client_ip = client_ip;
        ctx.plan = Some(plan);
        ctx._connection_permit = Some(permit);
        Ok(false)
    }

    async fn upstream_peer(
        &self,
        _session: &mut Session,
        ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>> {
        let plan = ctx
            .plan
            .as_ref()
            .ok_or_else(|| Error::explain(HTTPStatus(500), "request plan is missing"))?;
        let upstream = self
            .runtime
            .upstream(&plan.upstream_name)
            .ok_or_else(|| Error::explain(HTTPStatus(502), "upstream is missing"))?;
        let sni = upstream.sni.clone().unwrap_or_default();
        let mut peer = HttpPeer::new(upstream.address.as_str(), upstream.tls, sni);
        peer.options.connection_timeout =
            Some(Duration::from_secs(upstream.connect_timeout_seconds));
        peer.options.total_connection_timeout =
            Some(Duration::from_secs(upstream.connect_timeout_seconds));
        let route_timeout = Duration::from_secs(plan.route.timeout_seconds());
        peer.options.read_timeout =
            Some(route_timeout.max(Duration::from_secs(upstream.read_timeout_seconds)));
        peer.options.write_timeout =
            Some(route_timeout.max(Duration::from_secs(upstream.write_timeout_seconds)));
        peer.options.idle_timeout = Some(Duration::from_secs(upstream.idle_timeout_seconds));
        peer.options.verify_cert = upstream.verify_certificate;
        peer.options.verify_hostname = upstream.verify_certificate;
        peer.options.alpn = ALPN::H1;
        peer.options.tcp_keepalive = Some(TcpKeepalive {
            idle: Duration::from_secs(60),
            interval: Duration::from_secs(10),
            count: 3,
            #[cfg(target_os = "linux")]
            user_timeout: Duration::from_secs(90),
        });
        Ok(Box::new(peer))
    }

    async fn upstream_request_filter(
        &self,
        session: &mut Session,
        upstream_request: &mut RequestHeader,
        ctx: &mut Self::CTX,
    ) -> Result<()> {
        let plan = ctx
            .plan
            .as_ref()
            .ok_or_else(|| Error::explain(HTTPStatus(500), "request plan is missing"))?;
        let client_ip = ctx.client_ip.to_string();
        let upstream_host = if plan.route == RouteClass::Doh {
            "direct.tae00217.cloud"
        } else {
            plan.domain.as_str()
        };

        upstream_request.remove_header(&FORWARDED);
        upstream_request.remove_header("x-forwarded-for");
        upstream_request.insert_header(HOST, upstream_host)?;
        upstream_request.insert_header("x-real-ip", client_ip.as_str())?;
        upstream_request.insert_header("x-forwarded-for", client_ip.as_str())?;
        upstream_request.insert_header("x-forwarded-host", plan.domain.as_str())?;
        upstream_request.insert_header("x-forwarded-port", if plan.tls { "443" } else { "80" })?;
        upstream_request
            .insert_header("x-forwarded-proto", if plan.tls { "https" } else { "http" })?;
        upstream_request.insert_header("x-forwarded-ssl", if plan.tls { "on" } else { "off" })?;

        let navidrome = matches!(
            plan.handler,
            HandlerKind::NavidromeMain | HandlerKind::NavidromeCdn
        );
        if navidrome {
            if let Some(value) = session.req_header().headers.get(ACCEPT_ENCODING) {
                upstream_request.insert_header(ACCEPT_ENCODING, value.clone())?;
            } else {
                upstream_request.remove_header(&ACCEPT_ENCODING);
            }
        } else {
            upstream_request.remove_header(&ACCEPT_ENCODING);
        }

        if plan.route == RouteClass::Doh {
            upstream_request.remove_header(&UPGRADE);
            upstream_request.remove_header(&CONNECTION);
        } else if let Some(upgrade) = session.req_header().headers.get(UPGRADE) {
            upstream_request.insert_header(UPGRADE, upgrade.clone())?;
            upstream_request.insert_header(CONNECTION, "upgrade")?;
        } else {
            upstream_request.remove_header(&UPGRADE);
            upstream_request.remove_header(&CONNECTION);
        }
        Ok(())
    }

    async fn request_body_filter(
        &self,
        _session: &mut Session,
        body: &mut Option<Bytes>,
        _end_of_stream: bool,
        ctx: &mut Self::CTX,
    ) -> Result<()> {
        ctx.body_bytes = ctx
            .body_bytes
            .saturating_add(body.as_ref().map_or(0, Bytes::len));
        if ctx
            .plan
            .as_ref()
            .is_some_and(|plan| ctx.body_bytes > plan.max_body_bytes)
        {
            return Err(Error::explain(HTTPStatus(413), "request body is too large"));
        }
        Ok(())
    }

    async fn response_filter(
        &self,
        _session: &mut Session,
        response: &mut ResponseHeader,
        ctx: &mut Self::CTX,
    ) -> Result<()> {
        let Some(plan) = ctx.plan.as_ref() else {
            return Ok(());
        };
        strip_upstream_headers(response);
        insert_security_headers(response, plan.handler, plan.tls)?;
        if plan.route == RouteClass::Doh {
            response.remove_header(&CACHE_CONTROL);
            response.remove_header(&EXPIRES);
            response.remove_header(&PRAGMA);
            response.remove_header(&http::header::ETAG);
            response.remove_header(&LAST_MODIFIED);
            response.insert_header(CACHE_CONTROL, "no-store")?;
        }
        Ok(())
    }

    fn fail_to_connect(
        &self,
        _session: &mut Session,
        _peer: &HttpPeer,
        ctx: &mut Self::CTX,
        mut error: Box<Error>,
    ) -> Box<Error> {
        if ctx.retries < 1 {
            ctx.retries += 1;
            error.set_retry(true);
        }
        error
    }

    async fn logging(&self, session: &mut Session, error: Option<&Error>, ctx: &mut Self::CTX) {
        let elapsed = ctx.started_at.elapsed();
        let status = session
            .response_written()
            .map_or(0, |response| response.status.as_u16());
        if let Some(error) = error {
            warn!(
                "proxy error client={} status={} elapsed_ms={} error={}",
                ctx.client_ip,
                status,
                elapsed.as_millis(),
                error
            );
        } else if self.runtime.config.server.access_log {
            info!(
                "client={} method={} uri={} status={} elapsed_ms={}",
                ctx.client_ip,
                session.req_header().method,
                session.req_header().uri,
                status,
                elapsed.as_millis()
            );
        }
    }
}

fn request_authority(request: &RequestHeader) -> Option<String> {
    if request.headers.get_all(HOST).iter().count() > 1 {
        return None;
    }
    request
        .headers
        .get(HOST)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
        .or_else(|| {
            request
                .uri
                .authority()
                .map(|value| value.as_str().to_string())
        })
}

fn content_length(request: &RequestHeader) -> Option<usize> {
    request
        .headers
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse().ok())
}

fn is_tls(session: &Session) -> bool {
    session
        .digest()
        .and_then(|digest| digest.ssl_digest.as_ref())
        .is_some()
}

fn vaultwarden_auth_path(path: &str) -> bool {
    [
        "/api/accounts/login",
        "/api/accounts/prelogin",
        "/identity/connect/token",
    ]
    .iter()
    .any(|prefix| {
        path == *prefix
            || path
                .strip_prefix(prefix)
                .is_some_and(|rest| rest.starts_with('/'))
    })
}

fn connection_limit(handler: HandlerKind, route: RouteClass) -> usize {
    match handler {
        HandlerKind::NavidromeCdn if route == RouteClass::NavidromeStream => 12,
        HandlerKind::NavidromeCdn => 48,
        HandlerKind::NavidromeMain if route == RouteClass::NavidromeStream => 10,
        HandlerKind::NavidromeMain => 24,
        HandlerKind::Vaultwarden => 12,
        HandlerKind::Couchdb => 24,
        HandlerKind::AdguardDns | HandlerKind::AdguardKorea => 96,
        HandlerKind::Static => 1,
    }
}

pub fn resolve_client_ip(
    runtime: &RuntimeConfig,
    peer_ip: IpAddr,
    forwarded_for: Option<&str>,
) -> IpAddr {
    if !runtime.is_trusted_proxy(peer_ip) {
        return peer_ip;
    }
    let Some(forwarded_for) = forwarded_for.filter(|value| value.len() <= 4096) else {
        return peer_ip;
    };
    let parsed = forwarded_for
        .split(',')
        .map(str::trim)
        .map(str::parse::<IpAddr>)
        .collect::<std::result::Result<Vec<_>, _>>();
    let Ok(chain) = parsed else {
        return peer_ip;
    };
    if chain.len() > 32 {
        return peer_ip;
    }

    let mut selected = peer_ip;
    for candidate in chain.into_iter().rev() {
        selected = candidate;
        if !runtime.is_trusted_proxy(candidate) {
            break;
        }
    }
    selected
}

fn strip_upstream_headers(response: &mut ResponseHeader) {
    for name in [
        "server",
        "x-powered-by",
        "alt-svc",
        "strict-transport-security",
        "x-content-type-options",
        "x-frame-options",
        "referrer-policy",
    ] {
        response.remove_header(name);
    }
}

fn insert_security_headers(
    response: &mut ResponseHeader,
    handler: HandlerKind,
    tls: bool,
) -> Result<()> {
    response.insert_header("x-content-type-options", "nosniff")?;
    if tls {
        response.insert_header(
            "strict-transport-security",
            "max-age=63072000; includeSubDomains; preload",
        )?;
    }
    if matches!(
        handler,
        HandlerKind::Static | HandlerKind::Vaultwarden | HandlerKind::Couchdb
    ) {
        response.insert_header("x-frame-options", "SAMEORIGIN")?;
        response.insert_header("referrer-policy", "strict-origin-when-cross-origin")?;
    }
    Ok(())
}

async fn send_empty(
    session: &mut Session,
    status: u16,
    handler: Option<HandlerKind>,
    tls: bool,
    headers: &[(&'static str, &str)],
) -> Result<bool> {
    let mut response = ResponseHeader::build(status, Some(headers.len() + 8)).unwrap();
    response.insert_header(CONTENT_LENGTH, "0")?;
    for (name, value) in headers {
        response.insert_header(*name, *value)?;
    }
    if let Some(handler) = handler {
        insert_security_headers(&mut response, handler, tls)?;
    }
    session
        .write_response_header(Box::new(response), true)
        .await?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, RuntimeConfig};

    fn runtime() -> RuntimeConfig {
        let config: Config = serde_yaml::from_str(
            r#"
server:
  http_listen: ["127.0.0.1:8080"]
  https_listen: []
  certificate: /tmp/cert.pem
  private_key: /tmp/key.pem
trusted_proxies:
  - "127.0.0.0/8"
  - "10.0.0.0/8"
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
        .unwrap();
        RuntimeConfig::new(config).unwrap()
    }

    #[test]
    fn ignores_spoofed_forwarded_for_from_untrusted_peer() {
        let runtime = runtime();
        let peer = "198.51.100.20".parse().unwrap();
        assert_eq!(resolve_client_ip(&runtime, peer, Some("192.0.2.10")), peer);
    }

    #[test]
    fn recursively_selects_first_untrusted_forwarded_address() {
        let runtime = runtime();
        let peer = "127.0.0.1".parse().unwrap();
        assert_eq!(
            resolve_client_ip(&runtime, peer, Some("192.0.2.10, 10.0.0.4")),
            "192.0.2.10".parse::<IpAddr>().unwrap()
        );
    }

    #[test]
    fn recognizes_only_complete_vaultwarden_auth_prefixes() {
        assert!(vaultwarden_auth_path("/api/accounts/login"));
        assert!(vaultwarden_auth_path("/identity/connect/token/extra"));
        assert!(!vaultwarden_auth_path("/api/accounts/login-evil"));
    }
}
