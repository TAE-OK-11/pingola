use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::{Arc, Once};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use bytes::Bytes;
use cloudflare_pingora::http::{RequestHeader, ResponseHeader};
use cloudflare_pingora::prelude::HttpPeer;
use cloudflare_pingora::protocols::{TcpKeepalive, ALPN};
use cloudflare_pingora::proxy::{ProxyHttp, Session};
use cloudflare_pingora::Error;
use cloudflare_pingora::ErrorType;
use cloudflare_pingora::ErrorType::HTTPStatus;
use cloudflare_pingora::Result;
use http::header::{
    ACCEPT_ENCODING, CACHE_CONTROL, CONNECTION, CONTENT_LENGTH, EXPIRES, FORWARDED, HOST,
    LAST_MODIFIED, PRAGMA, TRANSFER_ENCODING, UPGRADE,
};
use http::Method;
use log::{info, warn};
use serde_json::json;

use crate::config::{HandlerKind, HostConfig, RuntimeConfig};
use crate::limits::{ActiveRequestLimiter, ActiveRequestPermit, RateLimiter};
use crate::static_files::StaticFiles;

const STREAM_PREFIXES: &[&str] = &[
    "/rest/stream",
    "/rest/download",
    "/stream",
    "/play",
    "/ext/stream",
];
const COVER_PREFIXES: &[&str] = &["/rest/getCoverArt", "/api/artwork", "/coverart", "/artwork"];
static LEGACY_HEALTH_WARNING: Once = Once::new();

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
    fn name(self) -> &'static str {
        match self {
            Self::NavidromeStream => "navidrome_stream",
            Self::NavidromeCover => "navidrome_cover",
            Self::NavidromeApi => "navidrome_api",
            Self::VaultwardenAuth => "vaultwarden_auth",
            Self::VaultwardenHub => "vaultwarden_hub",
            Self::Vaultwarden => "vaultwarden",
            Self::Couchdb => "couchdb",
            Self::Doh => "doh",
            Self::AdguardUi => "adguard_ui",
        }
    }

    fn default_rate_limit(self) -> Option<(f64, u32)> {
        match self {
            Self::NavidromeStream => Some((40.0, 15)),
            Self::NavidromeCover => Some((20.0, 20)),
            Self::NavidromeApi => Some((20.0, 30)),
            Self::VaultwardenAuth => Some((5.0 / 60.0, 3)),
            Self::Doh => Some((100.0, 200)),
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

    fn upstream_pool_group(self) -> u64 {
        match self {
            Self::NavidromeStream => 1,
            Self::NavidromeCover => 2,
            Self::NavidromeApi => 3,
            Self::VaultwardenAuth => 4,
            Self::VaultwardenHub => 5,
            Self::Vaultwarden => 6,
            Self::Couchdb => 7,
            Self::Doh => 8,
            Self::AdguardUi => 9,
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
    _active_request_permit: Option<ActiveRequestPermit>,
    _global_request_permit: Option<ActiveRequestPermit>,
}

impl Default for RequestContext {
    fn default() -> Self {
        Self {
            plan: None,
            client_ip: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            body_bytes: 0,
            retries: 0,
            started_at: Instant::now(),
            _active_request_permit: None,
            _global_request_permit: None,
        }
    }
}

pub struct Gateway {
    runtime: Arc<RuntimeConfig>,
    static_files: StaticFiles,
    rates: RateLimiter,
    active_requests: ActiveRequestLimiter,
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
            active_requests: ActiveRequestLimiter::new(),
        })
    }

    fn request_plan(
        &self,
        domain: &str,
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
            domain: domain.to_owned(),
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
        let path = session.req_header().uri.path();

        if path == "/pingora-health" || path == "/pingora-live" || path == "/pingora-ready" {
            return send_empty(session, 204, None, tls, &[("x-proxy-product", "Pingora")]).await;
        }
        if path == "/pingola-health" {
            if self.runtime.config.server.legacy_pingola_health {
                LEGACY_HEALTH_WARNING.call_once(|| {
                    warn!(
                        "/pingola-health is deprecated; migrate to /pingora-health (legacy support will be removed after one release)"
                    );
                });
                return send_empty(
                    session,
                    204,
                    None,
                    tls,
                    &[("x-proxy-product", "Pingora"), ("deprecation", "true")],
                )
                .await;
            }
            return send_empty(session, 404, None, tls, &[("x-proxy-product", "Pingora")]).await;
        }
        if path == "/nginx-health" {
            return send_empty(session, 404, None, tls, &[("x-proxy-product", "Pingora")]).await;
        }
        if path == "/pingora-health/details" {
            if !self.runtime.config.server.health_details {
                return send_empty(session, 404, None, tls, &[("x-proxy-product", "Pingora")])
                    .await;
            }
            return send_health_details(session, &self.runtime).await;
        }

        let Some(authority) = request_authority(session.req_header()) else {
            return send_empty(session, 400, None, tls, &[]).await;
        };
        let Some((domain, host_name, host)) = self.runtime.host(authority) else {
            session.set_keepalive(None);
            return send_empty(session, 421, None, tls, &[]).await;
        };

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
            return self.static_files.serve(host_name, session, tls).await;
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
        let Some(plan) = self.request_plan(domain, host, path, tls) else {
            return send_empty(session, 500, Some(host.handler), tls, &[]).await;
        };
        ctx.client_ip = client_ip;

        if content_length(session.req_header()).is_some_and(|length| length > plan.max_body_bytes) {
            return send_empty(session, 413, Some(plan.handler), tls, &[]).await;
        }

        if let Some((rate, burst)) = effective_rate_limit(&self.runtime, plan.route) {
            if !self.rates.allow(plan.route.name(), client_ip, rate, burst) {
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

        if self.runtime.config.server.global_active_requests > 0 {
            let Some(permit) = self.active_requests.acquire(
                "global",
                client_ip,
                self.runtime.config.server.global_active_requests,
            ) else {
                return send_empty(
                    session,
                    429,
                    Some(plan.handler),
                    tls,
                    &[("retry-after", "1")],
                )
                .await;
            };
            ctx._global_request_permit = Some(permit);
        }

        if let Some(active_limit) = effective_active_limit(&self.runtime, plan.handler, plan.route)
        {
            let Some(permit) =
                self.active_requests
                    .acquire(plan.route.name(), client_ip, active_limit)
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
            ctx._active_request_permit = Some(permit);
        }

        let timeout = Duration::from_secs(plan.route.timeout_seconds());
        session.set_read_timeout(Some(timeout));
        session.set_write_timeout(Some(timeout));
        session.set_keepalive(Some(30));
        session.set_keepalive_reuses_remaining(Some(500));
        ctx.plan = Some(plan);

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
        peer.group_key = plan.route.upstream_pool_group();
        peer.options.connection_timeout =
            Some(Duration::from_secs(upstream.connect_timeout_seconds));
        peer.options.total_connection_timeout =
            Some(Duration::from_secs(upstream.connect_timeout_seconds));
        let (read_timeout, write_timeout) = upstream_timeouts(plan.route, upstream);
        peer.options.read_timeout = Some(read_timeout);
        peer.options.write_timeout = Some(write_timeout);
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

        let navidrome_compressible = matches!(
            plan.route,
            RouteClass::NavidromeApi | RouteClass::NavidromeCover
        );
        if navidrome_compressible {
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
        session: &mut Session,
        _peer: &HttpPeer,
        ctx: &mut Self::CTX,
        mut error: Box<Error>,
    ) -> Box<Error> {
        let retryable_error = matches!(
            error.etype(),
            ErrorType::ConnectTimedout
                | ErrorType::ConnectRefused
                | ErrorType::ConnectNoRoute
                | ErrorType::ConnectError
                | ErrorType::TLSHandshakeTimedout
        );
        let should_retry = retryable_error
            && request_is_replay_safe(session)
            && ctx.retries < self.runtime.config.server.max_retries;
        warn!(
            "upstream connect failure category={} attempt={} configured_retries={} retry={} method={}",
            error.etype().as_str(),
            ctx.retries + 1,
            self.runtime.config.server.max_retries,
            should_retry,
            session.req_header().method
        );
        if should_retry {
            ctx.retries += 1;
            error.set_retry(true);
        } else {
            error.set_retry(false);
        }
        error
    }

    fn error_while_proxy(
        &self,
        peer: &HttpPeer,
        session: &mut Session,
        mut error: Box<Error>,
        ctx: &mut Self::CTX,
        client_reused: bool,
    ) -> Box<Error> {
        let can_retry = client_reused
            && request_is_replay_safe(session)
            && session.response_written().is_none()
            && !session.as_ref().retry_buffer_truncated()
            && ctx.retries < self.runtime.config.server.max_retries;
        error.retry.decide_reuse(can_retry);
        let should_retry = can_retry && error.retry.retry();
        error.set_retry(should_retry);
        if should_retry {
            ctx.retries += 1;
            warn!(
                "upstream reused-connection retry category={} retry={}/{} method={} peer={}",
                error.etype().as_str(),
                ctx.retries,
                self.runtime.config.server.max_retries,
                session.req_header().method,
                peer
            );
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
                "proxy error client={} status={} retries={} elapsed_ms={} error={}",
                ctx.client_ip,
                status,
                ctx.retries,
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

fn request_authority(request: &RequestHeader) -> Option<&str> {
    if request.headers.get_all(HOST).iter().count() > 1 {
        return None;
    }
    request
        .headers
        .get(HOST)
        .and_then(|value| value.to_str().ok())
        .or_else(|| request.uri.authority().map(|value| value.as_str()))
}

fn content_length(request: &RequestHeader) -> Option<usize> {
    request
        .headers
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse().ok())
}

fn request_is_replay_safe(session: &Session) -> bool {
    request_header_is_replay_safe(session.req_header())
}

fn request_header_is_replay_safe(request: &RequestHeader) -> bool {
    matches!(request.method, Method::GET | Method::HEAD)
        && content_length(request).is_none_or(|length| length == 0)
        && !request.headers.contains_key(TRANSFER_ENCODING)
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

fn default_active_limit(handler: HandlerKind, route: RouteClass) -> usize {
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

fn effective_active_limit(
    runtime: &RuntimeConfig,
    handler: HandlerKind,
    route: RouteClass,
) -> Option<usize> {
    let configured = runtime
        .config
        .route_limits
        .get(route.name())
        .and_then(|limit| limit.active_requests);
    let limit = configured.unwrap_or_else(|| default_active_limit(handler, route));
    (limit > 0).then_some(limit)
}

fn effective_rate_limit(runtime: &RuntimeConfig, route: RouteClass) -> Option<(f64, u32)> {
    let defaults = route.default_rate_limit();
    let configured = runtime.config.route_limits.get(route.name());
    let rate = configured
        .and_then(|limit| limit.rate_per_second)
        .or_else(|| defaults.map(|(rate, _)| rate))?;
    if rate == 0.0 {
        return None;
    }
    let burst = configured
        .and_then(|limit| limit.burst)
        .or_else(|| defaults.map(|(_, burst)| burst))
        .unwrap_or(0);
    Some((rate, burst))
}

fn upstream_timeouts(
    route: RouteClass,
    upstream: &crate::config::UpstreamConfig,
) -> (Duration, Duration) {
    let route_default = Duration::from_secs(route.timeout_seconds());
    (
        upstream
            .read_timeout_seconds
            .map(Duration::from_secs)
            .unwrap_or(route_default),
        upstream
            .write_timeout_seconds
            .map(Duration::from_secs)
            .unwrap_or(route_default),
    )
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

async fn send_health_details(session: &mut Session, runtime: &RuntimeConfig) -> Result<bool> {
    let query = session.req_header().uri.query().unwrap_or_default();
    let check_upstreams = query.split('&').any(|value| value == "upstreams=1");
    let allocator = if query.split('&').any(|value| value == "allocator=1")
        && crate::allocator::environment_requests_stats()
    {
        Some(crate::allocator::detailed_stats().map_err(|error| {
            Error::because(
                HTTPStatus(500),
                "allocator diagnostic collection failed",
                error,
            )
        })?)
    } else {
        None
    };
    let mut upstreams = std::collections::BTreeMap::new();
    let mut ready = true;
    if check_upstreams {
        for (name, upstream) in &runtime.config.upstreams {
            let connected = tokio::time::timeout(
                Duration::from_millis(500),
                tokio::net::TcpStream::connect(upstream.address.as_str()),
            )
            .await
            .is_ok_and(|result| result.is_ok());
            ready &= connected;
            upstreams.insert(name.as_str(), connected);
        }
    }
    let body = serde_json::to_vec(&json!({
        "product": "Pingora",
        "liveness": true,
        "readiness": ready,
        "listeners": {
            "http": runtime.config.server.http_listen,
            "https": runtime.config.server.https_listen,
        },
        "certificate_loaded": !runtime.config.server.https_listen.is_empty(),
        "upstreams_checked": check_upstreams,
        "upstreams": upstreams,
        "allocator": allocator,
    }))
    .map_err(|error| Error::because(HTTPStatus(500), "health JSON serialization failed", error))?;
    let mut response = ResponseHeader::build(if ready { 200 } else { 503 }, Some(8)).unwrap();
    response.insert_header("content-type", "application/json")?;
    response.insert_header(CONTENT_LENGTH, body.len().to_string())?;
    response.insert_header("cache-control", "no-store")?;
    response.insert_header("x-proxy-product", "Pingora")?;
    session
        .write_response_header(Box::new(response), body.is_empty())
        .await?;
    if !body.is_empty() {
        session
            .write_response_body(Some(Bytes::from(body)), true)
            .await?;
    }
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

    #[test]
    fn retries_only_bodyless_get_and_head_requests() {
        let get = RequestHeader::build(Method::GET, b"/", None).unwrap();
        assert!(request_header_is_replay_safe(&get));

        let mut get_with_body = RequestHeader::build(Method::GET, b"/", None).unwrap();
        get_with_body.insert_header(CONTENT_LENGTH, "1").unwrap();
        assert!(!request_header_is_replay_safe(&get_with_body));

        let post = RequestHeader::build(Method::POST, b"/", None).unwrap();
        assert!(!request_header_is_replay_safe(&post));
        let put = RequestHeader::build(Method::PUT, b"/", None).unwrap();
        assert!(!request_header_is_replay_safe(&put));
    }

    #[test]
    fn explicit_upstream_timeout_overrides_long_route_default() {
        let upstream: crate::config::UpstreamConfig = serde_yaml::from_str(
            r#"
address: "127.0.0.1:9000"
read_timeout_seconds: 7
write_timeout_seconds: 9
"#,
        )
        .unwrap();
        assert_eq!(
            upstream_timeouts(RouteClass::NavidromeStream, &upstream),
            (Duration::from_secs(7), Duration::from_secs(9))
        );
        assert_eq!(
            upstream_timeouts(RouteClass::VaultwardenHub, &upstream),
            (Duration::from_secs(7), Duration::from_secs(9))
        );
    }

    #[test]
    fn omitted_upstream_timeout_uses_each_route_default() {
        let upstream: crate::config::UpstreamConfig =
            serde_yaml::from_str("address: 127.0.0.1:9000").unwrap();
        assert_eq!(
            upstream_timeouts(RouteClass::NavidromeStream, &upstream),
            (Duration::from_secs(3600), Duration::from_secs(3600))
        );
        assert_eq!(
            upstream_timeouts(RouteClass::VaultwardenHub, &upstream),
            (Duration::from_secs(86_400), Duration::from_secs(86_400))
        );
        assert_eq!(
            upstream_timeouts(RouteClass::Doh, &upstream),
            (Duration::from_secs(30), Duration::from_secs(30))
        );
    }
}
