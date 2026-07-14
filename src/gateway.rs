use std::collections::HashMap;
use std::fmt::Write as _;
use std::net::{IpAddr, Ipv4Addr, ToSocketAddrs};
use std::sync::{Arc, Once};
use std::time::{Duration, Instant};

use ahash::AHashMap;
use anyhow::{anyhow, Context};
use arrayvec::ArrayString;
use async_trait::async_trait;
use bytes::Bytes;
use cloudflare_pingora::http::{RequestHeader, ResponseHeader};
use cloudflare_pingora::modules::http::compression::ResponseCompression;
use cloudflare_pingora::prelude::HttpPeer;
use cloudflare_pingora::protocols::{TcpKeepalive, ALPN};
use cloudflare_pingora::proxy::{ProxyHttp, Session};
use cloudflare_pingora::Error;
use cloudflare_pingora::ErrorType;
use cloudflare_pingora::ErrorType::HTTPStatus;
use cloudflare_pingora::Result;
use http::header::{
    HeaderName, HeaderValue, ACCEPT_ENCODING, CACHE_CONTROL, CONNECTION, CONTENT_ENCODING,
    CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, EXPIRES, FORWARDED, HOST, LAST_MODIFIED, PRAGMA,
    TRANSFER_ENCODING, UPGRADE,
};
use http::{Method, Version};
use log::{info, warn};
use serde_json::json;

use crate::config::{normalized_host, HandlerKind, RuntimeConfig, UpstreamProtocol};
use crate::content_encoding::{negotiate, ContentCoding, EncodingNegotiation};
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
const X_REAL_IP: HeaderName = HeaderName::from_static("x-real-ip");
const X_FORWARDED_FOR: HeaderName = HeaderName::from_static("x-forwarded-for");
const X_FORWARDED_HOST: HeaderName = HeaderName::from_static("x-forwarded-host");
const X_FORWARDED_PORT: HeaderName = HeaderName::from_static("x-forwarded-port");
const X_FORWARDED_PROTO: HeaderName = HeaderName::from_static("x-forwarded-proto");
const X_FORWARDED_SSL: HeaderName = HeaderName::from_static("x-forwarded-ssl");
const KEEP_ALIVE: HeaderName = HeaderName::from_static("keep-alive");
const PROXY_CONNECTION: HeaderName = HeaderName::from_static("proxy-connection");
const PROXY_AUTHENTICATE: HeaderName = HeaderName::from_static("proxy-authenticate");
const PROXY_AUTHORIZATION: HeaderName = HeaderName::from_static("proxy-authorization");
const TE: HeaderName = HeaderName::from_static("te");
const TRAILER: HeaderName = HeaderName::from_static("trailer");
const X_CONTENT_TYPE_OPTIONS: HeaderName = HeaderName::from_static("x-content-type-options");
const STRICT_TRANSPORT_SECURITY: HeaderName = HeaderName::from_static("strict-transport-security");
const X_FRAME_OPTIONS: HeaderName = HeaderName::from_static("x-frame-options");
const REFERRER_POLICY: HeaderName = HeaderName::from_static("referrer-policy");
const DIRECT_DOH_HOST: HeaderValue = HeaderValue::from_static("direct.tae00217.cloud");
const PORT_443: HeaderValue = HeaderValue::from_static("443");
const PORT_80: HeaderValue = HeaderValue::from_static("80");
const HTTPS: HeaderValue = HeaderValue::from_static("https");
const HTTP: HeaderValue = HeaderValue::from_static("http");
const ON: HeaderValue = HeaderValue::from_static("on");
const OFF: HeaderValue = HeaderValue::from_static("off");
const UPGRADE_VALUE: HeaderValue = HeaderValue::from_static("upgrade");
const NO_STORE: HeaderValue = HeaderValue::from_static("no-store");
const NOSNIFF: HeaderValue = HeaderValue::from_static("nosniff");
const HSTS_VALUE: HeaderValue =
    HeaderValue::from_static("max-age=63072000; includeSubDomains; preload");
const SAMEORIGIN: HeaderValue = HeaderValue::from_static("SAMEORIGIN");
const REFERRER_POLICY_VALUE: HeaderValue =
    HeaderValue::from_static("strict-origin-when-cross-origin");
const STRIPPED_RESPONSE_HEADERS: [HeaderName; 7] = [
    HeaderName::from_static("server"),
    HeaderName::from_static("x-powered-by"),
    HeaderName::from_static("alt-svc"),
    STRICT_TRANSPORT_SECURITY,
    X_CONTENT_TYPE_OPTIONS,
    X_FRAME_OPTIONS,
    REFERRER_POLICY,
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(usize)]
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
    const ALL: [Self; 9] = [
        Self::NavidromeStream,
        Self::NavidromeCover,
        Self::NavidromeApi,
        Self::VaultwardenAuth,
        Self::VaultwardenHub,
        Self::Vaultwarden,
        Self::Couchdb,
        Self::Doh,
        Self::AdguardUi,
    ];

    fn index(self) -> usize {
        self as usize
    }

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

#[derive(Clone, Debug)]
struct PreparedUpstream {
    peer: HttpPeer,
    read_timeout_seconds: Option<u64>,
    write_timeout_seconds: Option<u64>,
}

#[derive(Clone, Debug)]
struct PreparedPlan {
    domain: http::HeaderValue,
    handler: HandlerKind,
    peer: HttpPeer,
    route: RouteClass,
    max_body_bytes: usize,
}

#[derive(Debug)]
struct PreparedHost {
    domain: Arc<str>,
    name: String,
    handler: HandlerKind,
    redirect_http: bool,
    plans: [Option<Arc<PreparedPlan>>; RouteClass::ALL.len()],
}

impl PreparedHost {
    fn plan(&self, path: &str) -> Option<Arc<PreparedPlan>> {
        let route = match self.handler {
            HandlerKind::Static => return None,
            HandlerKind::NavidromeMain | HandlerKind::NavidromeCdn => {
                if STREAM_PREFIXES
                    .iter()
                    .any(|prefix| path.starts_with(prefix))
                {
                    RouteClass::NavidromeStream
                } else if COVER_PREFIXES.iter().any(|prefix| path.starts_with(prefix)) {
                    RouteClass::NavidromeCover
                } else {
                    RouteClass::NavidromeApi
                }
            }
            HandlerKind::Vaultwarden => {
                if vaultwarden_auth_path(path) {
                    RouteClass::VaultwardenAuth
                } else if path.starts_with("/notifications/hub") {
                    RouteClass::VaultwardenHub
                } else {
                    RouteClass::Vaultwarden
                }
            }
            HandlerKind::Couchdb => RouteClass::Couchdb,
            HandlerKind::AdguardDns | HandlerKind::AdguardKorea => {
                if path.starts_with("/dns-query") {
                    RouteClass::Doh
                } else {
                    RouteClass::AdguardUi
                }
            }
        };
        self.plans[route.index()].clone()
    }
}

#[derive(Clone, Copy)]
struct RoutePolicy {
    rate_limit: Option<(f64, u32)>,
    active_request_override: Option<usize>,
}

pub struct RequestContext {
    plan: Option<Arc<PreparedPlan>>,
    client_ip: IpAddr,
    tls: bool,
    body_bytes: usize,
    retries: usize,
    identity_acceptable: bool,
    started_at: Option<Instant>,
    _active_request_permit: Option<ActiveRequestPermit>,
    _global_request_permit: Option<ActiveRequestPermit>,
}

impl Default for RequestContext {
    fn default() -> Self {
        Self {
            plan: None,
            client_ip: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            tls: false,
            body_bytes: 0,
            retries: 0,
            identity_acceptable: true,
            started_at: None,
            _active_request_permit: None,
            _global_request_permit: None,
        }
    }
}

pub struct Gateway {
    runtime: Arc<RuntimeConfig>,
    static_files: StaticFiles,
    hosts: AHashMap<Arc<str>, PreparedHost>,
    route_policies: [RoutePolicy; RouteClass::ALL.len()],
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
        let upstreams = runtime
            .config
            .upstreams
            .iter()
            .map(|(name, upstream)| {
                prepare_upstream(name, upstream).map(|prepared| (name.clone(), prepared))
            })
            .collect::<anyhow::Result<HashMap<_, _>>>()?;
        let mut hosts = AHashMap::with_capacity(
            runtime
                .config
                .hosts
                .values()
                .map(|host| host.domains.len())
                .sum(),
        );
        for (name, host) in &runtime.config.hosts {
            for domain in &host.domains {
                let canonical_domain: Arc<str> = Arc::from(domain.as_str());
                let domain_header = http::HeaderValue::from_str(domain).with_context(|| {
                    format!(
                        "host domain cannot be encoded as a header: host={name} domain={domain}"
                    )
                })?;
                let plans = RouteClass::ALL.map(|route| {
                    let upstream_name =
                        upstream_name_for_route(host.handler, host.upstream.as_deref(), route)?;
                    let upstream = upstreams.get(upstream_name)?;
                    Some(Arc::new(PreparedPlan {
                        domain: domain_header.clone(),
                        handler: host.handler,
                        peer: prepare_route_peer(upstream, route),
                        route,
                        max_body_bytes: host.max_body_bytes,
                    }))
                });
                hosts.insert(
                    canonical_domain.clone(),
                    PreparedHost {
                        domain: canonical_domain,
                        name: name.clone(),
                        handler: host.handler,
                        redirect_http: host.redirect_http,
                        plans,
                    },
                );
            }
        }
        let route_policies = RouteClass::ALL.map(|route| RoutePolicy {
            rate_limit: effective_rate_limit(&runtime, route),
            active_request_override: runtime
                .config
                .route_limits
                .get(route.name())
                .and_then(|limit| limit.active_requests),
        });
        Ok(Self {
            runtime,
            static_files,
            hosts,
            route_policies,
            rates: RateLimiter::new(),
            active_requests: ActiveRequestLimiter::new(),
        })
    }

    fn host(&self, authority: &str) -> Option<&PreparedHost> {
        let domain = normalized_host(authority);
        self.hosts.get::<str>(domain.as_ref())
    }

    fn acquire_global_request(&self, ctx: &mut RequestContext) -> bool {
        let limit = self.runtime.config.server.global_active_requests;
        if limit == 0 {
            return true;
        }
        let Some(permit) = self.active_requests.acquire("global", ctx.client_ip, limit) else {
            return false;
        };
        ctx._global_request_permit = Some(permit);
        true
    }
}

#[async_trait]
impl ProxyHttp for Gateway {
    type CTX = RequestContext;

    fn new_ctx(&self) -> Self::CTX {
        RequestContext {
            started_at: self.runtime.config.server.access_log.then(Instant::now),
            ..RequestContext::default()
        }
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
        let Some(host) = self.host(authority) else {
            session.set_keepalive(None);
            return send_empty(session, 421, None, tls, &[]).await;
        };

        if !tls && host.redirect_http {
            let path_and_query = session
                .req_header()
                .uri
                .path_and_query()
                .map_or("/", |value| value.as_str());
            let location = format!("https://{}{path_and_query}", host.domain.as_ref());
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
            if self.runtime.config.server.global_active_requests > 0 {
                ctx.client_ip = session_client_ip(&self.runtime, session);
            }
            if !self.acquire_global_request(ctx) {
                return send_empty(
                    session,
                    429,
                    Some(host.handler),
                    tls,
                    &[("retry-after", "1")],
                )
                .await;
            }
            return self.static_files.serve(&host.name, session, tls).await;
        }

        let client_ip = session_client_ip(&self.runtime, session);
        ctx.client_ip = client_ip;

        if host.handler == HandlerKind::NavidromeMain && path == "/" {
            let location = format!("https://{}/app/", host.domain.as_ref());
            return send_empty(
                session,
                308,
                Some(host.handler),
                tls,
                &[("location", location.as_str())],
            )
            .await;
        }

        let Some(plan) = host.plan(path) else {
            return send_empty(session, 500, Some(host.handler), tls, &[]).await;
        };
        let encoding = configure_downstream_compression(session, plan.route)?;
        if encoding.preferred == ContentCoding::NotAcceptable {
            return send_empty(session, 406, Some(plan.handler), tls, &[]).await;
        }
        ctx.identity_acceptable = encoding.identity_acceptable;
        ctx.tls = tls;

        if content_length(session.req_header()).is_some_and(|length| length > plan.max_body_bytes) {
            return send_empty(session, 413, Some(plan.handler), tls, &[]).await;
        }

        let policy = self.route_policies[plan.route.index()];
        if let Some((rate, burst)) = policy.rate_limit {
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

        if !self.acquire_global_request(ctx) {
            return send_empty(
                session,
                429,
                Some(plan.handler),
                tls,
                &[("retry-after", "1")],
            )
            .await;
        }

        let active_limit = policy
            .active_request_override
            .unwrap_or_else(|| default_active_limit(plan.handler, plan.route));
        if active_limit > 0 {
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
        Ok(Box::new(plan.peer.clone()))
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
        strip_request_hop_headers(session.req_header(), upstream_request)?;
        let mut client_ip_text = ArrayString::<64>::new();
        write!(&mut client_ip_text, "{}", ctx.client_ip).map_err(|error| {
            Error::because(
                HTTPStatus(400),
                "resolved client IP could not be formatted as a header",
                error,
            )
        })?;
        let client_ip = http::HeaderValue::from_str(&client_ip_text).map_err(|error| {
            Error::because(
                HTTPStatus(400),
                "resolved client IP could not be encoded as a header",
                error,
            )
        })?;
        let upstream_host = if plan.route == RouteClass::Doh {
            DIRECT_DOH_HOST
        } else {
            plan.domain.clone()
        };

        upstream_request.remove_header(&FORWARDED);
        upstream_request.remove_header(&X_FORWARDED_FOR);
        upstream_request.insert_header(HOST, upstream_host)?;
        upstream_request.insert_header(X_REAL_IP, client_ip.clone())?;
        upstream_request.insert_header(X_FORWARDED_FOR, client_ip)?;
        upstream_request.insert_header(X_FORWARDED_HOST, plan.domain.clone())?;
        let listener_port = session
            .server_addr()
            .and_then(|address| address.as_inet())
            .map(|address| address.port());
        upstream_request.insert_header(
            X_FORWARDED_PORT,
            forwarded_port_value(listener_port, ctx.tls)?,
        )?;
        upstream_request.insert_header(X_FORWARDED_PROTO, if ctx.tls { HTTPS } else { HTTP })?;
        upstream_request.insert_header(X_FORWARDED_SSL, if ctx.tls { ON } else { OFF })?;

        if forwards_accept_encoding(plan.route) {
            if let Some(value) = session.req_header().headers.get(ACCEPT_ENCODING) {
                upstream_request.insert_header(ACCEPT_ENCODING, value.clone())?;
            } else {
                upstream_request.remove_header(&ACCEPT_ENCODING);
            }
        } else {
            upstream_request.remove_header(&ACCEPT_ENCODING);
        }

        let forwards_upgrade = plan.route != RouteClass::Doh
            && upstream_request.version == Version::HTTP_11
            && session.is_upgrade_req();
        if forwards_upgrade {
            let upgrade = session.req_header().headers.get(UPGRADE).ok_or_else(|| {
                Error::explain(
                    HTTPStatus(400),
                    "upgrade request is missing its Upgrade header",
                )
            })?;
            upstream_request.insert_header(UPGRADE, upgrade.clone())?;
            upstream_request.insert_header(CONNECTION, UPGRADE_VALUE)?;
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
        session: &mut Session,
        response: &mut ResponseHeader,
        ctx: &mut Self::CTX,
    ) -> Result<()> {
        let Some(plan) = ctx.plan.as_ref() else {
            return Ok(());
        };
        let forwards_upgrade = response.status.as_u16() == 101
            && session.req_header().version == Version::HTTP_11
            && session.is_upgrade_req();
        strip_response_hop_headers(response, forwards_upgrade)?;
        strip_upstream_headers(response);
        insert_security_headers(response, plan.handler, ctx.tls)?;
        if uses_downstream_compression(plan.route)
            && response_status_is_interim(response.status.as_u16())
        {
            // 100/103 are interim headers. Do not permanently disable the
            // compressor before the final response arrives.
            return Ok(());
        }
        // Status-defined no-content responses carry no selected representation.
        // HEAD still describes the corresponding GET representation, so it
        // must follow the same content-coding acceptability decision as GET.
        let bodyless = response_status_has_no_body(response.status.as_u16());
        if uses_downstream_compression(plan.route)
            && (bodyless || !response_allows_compression(response))
        {
            if let Some(compression) = session
                .downstream_modules_ctx
                .get_mut::<ResponseCompression>()
            {
                compression.adjust_level(0);
            }
            if !bodyless && !ctx.identity_acceptable {
                return Err(Error::explain(
                    HTTPStatus(406),
                    "upstream response cannot use an acceptable content coding",
                ));
            }
        }
        if plan.route == RouteClass::Doh {
            response.remove_header(&CACHE_CONTROL);
            response.remove_header(&EXPIRES);
            response.remove_header(&PRAGMA);
            response.remove_header(&http::header::ETAG);
            response.remove_header(&LAST_MODIFIED);
            response.insert_header(CACHE_CONTROL, NO_STORE)?;
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
        let status = session
            .response_written()
            .map_or(0, |response| response.status.as_u16());
        if let Some(error) = error {
            if let Some(started_at) = ctx.started_at {
                warn!(
                    "proxy error client={} status={} retries={} elapsed_ms={} error={}",
                    ctx.client_ip,
                    status,
                    ctx.retries,
                    started_at.elapsed().as_millis(),
                    error
                );
            } else {
                warn!(
                    "proxy error client={} status={} retries={} error={}",
                    ctx.client_ip, status, ctx.retries, error
                );
            }
        } else if self.runtime.config.server.access_log {
            let elapsed = ctx
                .started_at
                .map_or(Duration::ZERO, |started_at| started_at.elapsed());
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

/// Let selected application origins negotiate their own response encoding.
///
/// Audio, binary DoH, authentication responses and upgraded/long-lived
/// connections deliberately stay uncompressed. Vaultwarden and CouchDB use a
/// separate bounded streaming compressor in the downstream response path.
fn forwards_accept_encoding(route: RouteClass) -> bool {
    matches!(route, RouteClass::NavidromeApi | RouteClass::NavidromeCover)
}

fn uses_downstream_compression(route: RouteClass) -> bool {
    matches!(route, RouteClass::Vaultwarden | RouteClass::Couchdb)
}

fn configure_downstream_compression(
    session: &mut Session,
    route: RouteClass,
) -> Result<EncodingNegotiation> {
    if !uses_downstream_compression(route) {
        return Ok(EncodingNegotiation {
            preferred: ContentCoding::Identity,
            identity_acceptable: true,
        });
    }

    let negotiation = negotiate(session.req_header().headers.get_all(ACCEPT_ENCODING).iter());
    let Some(encoding) = negotiation.preferred.as_str() else {
        return Ok(negotiation);
    };

    // The default Pingora module starts disabled, so it skipped its earlier
    // request-header hook. Normalize all q-values and duplicate fields to one
    // accepted coding before feeding Pingora's parser, which currently ignores
    // q-values itself.
    session
        .downstream_session
        .req_header_mut()
        .insert_header(ACCEPT_ENCODING, encoding)?;
    let request = session.downstream_session.req_header();
    if let Some(compression) = session
        .downstream_modules_ctx
        .get_mut::<ResponseCompression>()
    {
        compression.adjust_level(1);
        compression.request_filter(request);
    }
    Ok(negotiation)
}

fn response_allows_compression(response: &ResponseHeader) -> bool {
    if response_status_has_no_body(response.status.as_u16())
        || response.status.as_u16() == 206
        || response.headers.contains_key(CONTENT_RANGE)
        || response.headers.contains_key(CONTENT_ENCODING)
    {
        return false;
    }
    if response.headers.get_all(CACHE_CONTROL).iter().any(|value| {
        value.to_str().is_ok_and(|value| {
            value.split(',').any(|directive| {
                directive
                    .trim()
                    .split_once('=')
                    .map_or(directive.trim(), |(name, _)| name.trim())
                    .eq_ignore_ascii_case("no-transform")
            })
        })
    }) {
        return false;
    }
    if let Some(length) = response.headers.get(CONTENT_LENGTH) {
        if length
            .to_str()
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .is_none_or(|length| length < 1024)
        {
            return false;
        }
    }

    response
        .headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(compressible_proxy_content_type)
}

fn response_status_has_no_body(status: u16) -> bool {
    (100..200).contains(&status) || status == 204 || status == 205 || status == 304
}

fn response_status_is_interim(status: u16) -> bool {
    (100..200).contains(&status) && status != 101
}

fn compressible_proxy_content_type(value: &str) -> bool {
    let essence = value.split(';').next().unwrap_or_default().trim();
    if essence
        .get(..5)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("text/"))
    {
        return true;
    }
    if [
        "application/javascript",
        "application/json",
        "application/ld+json",
        "application/manifest+json",
        "application/xhtml+xml",
        "application/xml",
        "application/rss+xml",
        "image/svg+xml",
    ]
    .iter()
    .any(|candidate| essence.eq_ignore_ascii_case(candidate))
    {
        return true;
    }
    essence.rsplit_once('+').is_some_and(|(_, suffix)| {
        suffix.eq_ignore_ascii_case("json") || suffix.eq_ignore_ascii_case("xml")
    })
}

fn connection_option_names(
    headers: &http::HeaderMap,
    invalid_status: u16,
) -> Result<Vec<HeaderName>> {
    let mut names = Vec::new();
    for field in [&CONNECTION, &PROXY_CONNECTION] {
        for value in headers.get_all(field).iter() {
            for token in value.as_bytes().split(|byte| *byte == b',') {
                let token = token.trim_ascii();
                if token.is_empty() {
                    continue;
                }
                let name = HeaderName::from_bytes(token).map_err(|error| {
                    Error::because(
                        HTTPStatus(invalid_status),
                        "invalid Connection header option",
                        error,
                    )
                })?;
                if name == CONTENT_LENGTH || name == TRANSFER_ENCODING || name == HOST {
                    return Err(Error::explain(
                        HTTPStatus(invalid_status),
                        format!("Connection header names critical framing field {name}"),
                    ));
                }
                names.push(name);
            }
        }
    }
    Ok(names)
}

fn strip_request_hop_headers(
    downstream: &RequestHeader,
    upstream: &mut RequestHeader,
) -> Result<()> {
    for name in connection_option_names(&downstream.headers, 400)? {
        upstream.remove_header(&name);
    }
    for name in [
        &CONNECTION,
        &KEEP_ALIVE,
        &PROXY_CONNECTION,
        &PROXY_AUTHENTICATE,
        &PROXY_AUTHORIZATION,
        &TE,
        &TRAILER,
        &UPGRADE,
    ] {
        upstream.remove_header(name);
    }
    Ok(())
}

fn strip_response_hop_headers(response: &mut ResponseHeader, forwards_upgrade: bool) -> Result<()> {
    let upgrade = forwards_upgrade
        .then(|| response.headers.get(UPGRADE).cloned())
        .flatten();
    for name in connection_option_names(&response.headers, 502)? {
        response.remove_header(&name);
    }
    for name in [
        &CONNECTION,
        &KEEP_ALIVE,
        &PROXY_CONNECTION,
        &PROXY_AUTHENTICATE,
        &PROXY_AUTHORIZATION,
        &TE,
        &TRAILER,
        &UPGRADE,
    ] {
        response.remove_header(name);
    }
    if forwards_upgrade {
        let upgrade = upgrade.ok_or_else(|| {
            Error::explain(
                HTTPStatus(502),
                "upstream 101 response is missing its Upgrade header",
            )
        })?;
        response.insert_header(UPGRADE, upgrade)?;
        response.insert_header(CONNECTION, UPGRADE_VALUE)?;
    }
    Ok(())
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

fn request_is_replay_safe(session: &mut Session) -> bool {
    request_header_is_replay_safe(session.req_header()) && session.as_mut().is_body_empty()
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

fn session_client_ip(runtime: &RuntimeConfig, session: &Session) -> IpAddr {
    let peer_ip = session
        .client_addr()
        .and_then(|address| address.as_inet())
        .map_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED), |address| address.ip());
    let forwarded_for = session
        .req_header()
        .headers
        .get("x-forwarded-for")
        .and_then(|value| value.to_str().ok());
    resolve_client_ip(runtime, peer_ip, forwarded_for)
}

fn forwarded_port_value(port: Option<u16>, tls: bool) -> Result<HeaderValue> {
    match port {
        Some(80) => Ok(PORT_80),
        Some(443) => Ok(PORT_443),
        Some(port) => {
            let mut value = ArrayString::<5>::new();
            write!(&mut value, "{port}").map_err(|error| {
                Error::because(
                    HTTPStatus(500),
                    "listener port could not be formatted as a header",
                    error,
                )
            })?;
            HeaderValue::from_str(&value).map_err(|error| {
                Error::because(
                    HTTPStatus(500),
                    "listener port could not be encoded as a header",
                    error,
                )
            })
        }
        None if tls => Ok(PORT_443),
        None => Ok(PORT_80),
    }
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
        HandlerKind::Static => 0,
    }
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

fn upstream_timeouts(route: RouteClass, upstream: &PreparedUpstream) -> (Duration, Duration) {
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

fn upstream_name_for_route(
    handler: HandlerKind,
    configured: Option<&str>,
    route: RouteClass,
) -> Option<&str> {
    match (handler, route) {
        (
            HandlerKind::NavidromeMain | HandlerKind::NavidromeCdn,
            RouteClass::NavidromeStream | RouteClass::NavidromeCover | RouteClass::NavidromeApi,
        )
        | (
            HandlerKind::Vaultwarden,
            RouteClass::VaultwardenAuth | RouteClass::VaultwardenHub | RouteClass::Vaultwarden,
        )
        | (HandlerKind::Couchdb, RouteClass::Couchdb)
        | (HandlerKind::AdguardDns | HandlerKind::AdguardKorea, RouteClass::AdguardUi) => {
            configured
        }
        (HandlerKind::AdguardDns, RouteClass::Doh) => Some("adguard_dns_doh"),
        (HandlerKind::AdguardKorea, RouteClass::Doh) => Some("adguard_korea_doh"),
        _ => None,
    }
}

fn prepare_route_peer(upstream: &PreparedUpstream, route: RouteClass) -> HttpPeer {
    let mut peer = upstream.peer.clone();
    peer.group_key = route.upstream_pool_group();
    // WebSocket Upgrade is an HTTP/1.1 hop-by-hop mechanism. Pingora does not
    // implement RFC 8441 extended CONNECT for this route.
    if route == RouteClass::VaultwardenHub {
        peer.options.alpn = ALPN::H1;
        peer.options.max_h2_streams = 1;
    }
    let (read_timeout, write_timeout) = upstream_timeouts(route, upstream);
    peer.options.read_timeout = Some(read_timeout);
    peer.options.write_timeout = Some(write_timeout);
    peer.cache_reuse_hash();
    peer
}

fn prepare_upstream(
    name: &str,
    upstream: &crate::config::UpstreamConfig,
) -> anyhow::Result<PreparedUpstream> {
    let address = upstream
        .address
        .to_socket_addrs()
        .with_context(|| {
            format!(
                "upstream address resolution failed: name={name} address={}",
                upstream.address
            )
        })?
        .next()
        .ok_or_else(|| {
            anyhow!(
                "upstream address resolution returned no addresses: name={name} address={}",
                upstream.address
            )
        })?;
    let mut peer = HttpPeer::new(
        address,
        upstream.tls,
        upstream.sni.clone().unwrap_or_default(),
    );
    peer.options.connection_timeout = Some(Duration::from_secs(upstream.connect_timeout_seconds));
    peer.options.total_connection_timeout =
        Some(Duration::from_secs(upstream.connect_timeout_seconds));
    peer.options.idle_timeout = Some(Duration::from_secs(upstream.idle_timeout_seconds));
    peer.options.verify_cert = upstream.verify_certificate;
    peer.options.verify_hostname = upstream.verify_certificate;
    peer.options.alpn = match upstream.protocol {
        UpstreamProtocol::Auto if upstream.tls => ALPN::H2H1,
        UpstreamProtocol::Auto | UpstreamProtocol::Http1 => ALPN::H1,
        UpstreamProtocol::Http2 => ALPN::H2,
    };
    peer.options.max_h2_streams = upstream.http2_max_concurrent_streams;
    peer.options.tcp_keepalive = Some(TcpKeepalive {
        idle: Duration::from_secs(60),
        interval: Duration::from_secs(10),
        count: 3,
        #[cfg(target_os = "linux")]
        user_timeout: Duration::from_secs(90),
    });
    Ok(PreparedUpstream {
        peer,
        read_timeout_seconds: upstream.read_timeout_seconds,
        write_timeout_seconds: upstream.write_timeout_seconds,
    })
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
    if forwarded_for.split(',').nth(32).is_some() {
        return peer_ip;
    }

    let mut selected = peer_ip;
    for candidate in forwarded_for.rsplit(',') {
        let Ok(candidate) = candidate.trim().parse::<IpAddr>() else {
            return peer_ip;
        };
        selected = candidate;
        if !runtime.is_trusted_proxy(candidate) {
            break;
        }
    }
    selected
}

fn strip_upstream_headers(response: &mut ResponseHeader) {
    for name in &STRIPPED_RESPONSE_HEADERS {
        response.remove_header(name);
    }
}

fn insert_security_headers(
    response: &mut ResponseHeader,
    handler: HandlerKind,
    tls: bool,
) -> Result<()> {
    response.insert_header(X_CONTENT_TYPE_OPTIONS, NOSNIFF)?;
    if tls {
        response.insert_header(STRICT_TRANSPORT_SECURITY, HSTS_VALUE)?;
    }
    if matches!(
        handler,
        HandlerKind::Static | HandlerKind::Vaultwarden | HandlerKind::Couchdb
    ) {
        response.insert_header(X_FRAME_OPTIONS, SAMEORIGIN)?;
        response.insert_header(REFERRER_POLICY, REFERRER_POLICY_VALUE)?;
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
        let config: Config = serde_saphyr::from_str(
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
    fn prepared_host_lookup_is_case_insensitive_and_peers_cache_pool_hash() {
        let gateway = Gateway::new(Arc::new(runtime())).unwrap();
        let host = gateway.host("APP.EXAMPLE.COM:443").unwrap();
        assert_eq!(host.domain.as_ref(), "app.example.com");
        let plan = host.plan("/rest/stream").unwrap();
        assert!(plan.peer.cached_reuse_hash.is_some());
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
    fn applies_compression_only_to_intended_routes() {
        for route in [RouteClass::NavidromeApi, RouteClass::NavidromeCover] {
            assert!(forwards_accept_encoding(route), "route={route:?}");
        }
        for route in [
            RouteClass::NavidromeStream,
            RouteClass::VaultwardenAuth,
            RouteClass::VaultwardenHub,
            RouteClass::Vaultwarden,
            RouteClass::Couchdb,
            RouteClass::Doh,
            RouteClass::AdguardUi,
        ] {
            assert!(!forwards_accept_encoding(route), "route={route:?}");
        }

        assert!(uses_downstream_compression(RouteClass::Vaultwarden));
        assert!(uses_downstream_compression(RouteClass::Couchdb));
        for route in [
            RouteClass::NavidromeStream,
            RouteClass::NavidromeCover,
            RouteClass::NavidromeApi,
            RouteClass::VaultwardenAuth,
            RouteClass::VaultwardenHub,
            RouteClass::Doh,
            RouteClass::AdguardUi,
        ] {
            assert!(!uses_downstream_compression(route), "route={route:?}");
        }
    }

    #[test]
    fn compression_gate_rejects_small_binary_partial_and_no_transform_responses() {
        let mut response = ResponseHeader::build(200, None).unwrap();
        response
            .insert_header(CONTENT_TYPE, "application/json")
            .unwrap();
        response.insert_header(CONTENT_LENGTH, "2048").unwrap();
        assert!(response_allows_compression(&response));

        response
            .insert_header(CACHE_CONTROL, "private, no-transform")
            .unwrap();
        assert!(!response_allows_compression(&response));
        response.remove_header(&CACHE_CONTROL);
        response.insert_header(CONTENT_LENGTH, "100").unwrap();
        assert!(!response_allows_compression(&response));
        response.insert_header(CONTENT_LENGTH, "2048").unwrap();
        response
            .insert_header(CONTENT_TYPE, "application/octet-stream")
            .unwrap();
        assert!(!response_allows_compression(&response));
        response.status = http::StatusCode::PARTIAL_CONTENT;
        assert!(!response_allows_compression(&response));
        response.status = http::StatusCode::OK;
        response.remove_header(&CONTENT_RANGE);
        response
            .insert_header(CONTENT_ENCODING, "already-encoded")
            .unwrap();
        assert!(!response_allows_compression(&response));
        response.remove_header(&CONTENT_ENCODING);
        response.status = http::StatusCode::NO_CONTENT;
        assert!(!response_allows_compression(&response));
        response.status = http::StatusCode::NOT_MODIFIED;
        assert!(!response_allows_compression(&response));
        response.status = http::StatusCode::RESET_CONTENT;
        assert!(!response_allows_compression(&response));
        assert!(response_status_is_interim(100));
        assert!(response_status_is_interim(103));
        assert!(!response_status_is_interim(101));
        assert!(!response_status_is_interim(200));
    }

    #[test]
    fn connection_nominated_and_fixed_hop_headers_are_removed() {
        let mut downstream = RequestHeader::build(Method::GET, b"/", None).unwrap();
        downstream
            .insert_header(CONNECTION, "keep-alive, x-private")
            .unwrap();
        downstream.insert_header("x-private", "secret").unwrap();
        downstream.insert_header(KEEP_ALIVE, "timeout=5").unwrap();
        downstream
            .insert_header(PROXY_AUTHORIZATION, "Basic secret")
            .unwrap();
        let mut upstream = downstream.clone();

        strip_request_hop_headers(&downstream, &mut upstream).unwrap();
        for name in [
            &CONNECTION,
            &KEEP_ALIVE,
            &PROXY_AUTHORIZATION,
            &HeaderName::from_static("x-private"),
        ] {
            assert!(!upstream.headers.contains_key(name));
        }
    }

    #[test]
    fn connection_option_cannot_hide_request_framing() {
        let mut downstream = RequestHeader::build(Method::POST, b"/", None).unwrap();
        downstream
            .insert_header(CONNECTION, "transfer-encoding")
            .unwrap();
        downstream
            .insert_header(TRANSFER_ENCODING, "chunked")
            .unwrap();
        let mut upstream = downstream.clone();
        assert!(strip_request_hop_headers(&downstream, &mut upstream).is_err());
    }

    #[test]
    fn response_hop_headers_are_removed_except_a_valid_h1_upgrade() {
        let mut response = ResponseHeader::build(200, None).unwrap();
        response
            .insert_header(CONNECTION, "keep-alive, x-private")
            .unwrap();
        response.insert_header("x-private", "secret").unwrap();
        response.insert_header(KEEP_ALIVE, "timeout=5").unwrap();
        response
            .insert_header(PROXY_AUTHENTICATE, "Basic realm=proxy")
            .unwrap();
        strip_response_hop_headers(&mut response, false).unwrap();
        for name in [
            &CONNECTION,
            &KEEP_ALIVE,
            &PROXY_AUTHENTICATE,
            &HeaderName::from_static("x-private"),
        ] {
            assert!(!response.headers.contains_key(name));
        }

        let mut switching = ResponseHeader::build(101, None).unwrap();
        switching.insert_header(CONNECTION, "upgrade").unwrap();
        switching.insert_header(UPGRADE, "websocket").unwrap();
        strip_response_hop_headers(&mut switching, true).unwrap();
        assert_eq!(switching.headers.get(CONNECTION).unwrap(), "upgrade");
        assert_eq!(switching.headers.get(UPGRADE).unwrap(), "websocket");
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
        let upstream: crate::config::UpstreamConfig = serde_saphyr::from_str(
            r#"
address: "127.0.0.1:9000"
read_timeout_seconds: 7
write_timeout_seconds: 9
"#,
        )
        .unwrap();
        let upstream = prepare_upstream("test", &upstream).unwrap();
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
            serde_saphyr::from_str("address: 127.0.0.1:9000").unwrap();
        let upstream = prepare_upstream("test", &upstream).unwrap();
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

    #[test]
    fn invalid_upstream_address_is_rejected_before_serving_requests() {
        let upstream: crate::config::UpstreamConfig =
            serde_saphyr::from_str("address: '127.0.0.1:not-a-port'").unwrap();
        let error = prepare_upstream("broken", &upstream).unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("name=broken"));
        assert!(message.contains("127.0.0.1:not-a-port"));
    }

    #[test]
    fn tls_upstream_auto_prefers_h2_with_h1_fallback() {
        let upstream: crate::config::UpstreamConfig =
            serde_saphyr::from_str("address: 127.0.0.1:9443\ntls: true\nsni: upstream.test")
                .unwrap();
        let prepared = prepare_upstream("test", &upstream).unwrap();
        assert_eq!(prepared.peer.options.alpn, ALPN::H2H1);
        assert_eq!(prepared.peer.options.max_h2_streams, 32);

        let hub = prepare_route_peer(&prepared, RouteClass::VaultwardenHub);
        assert_eq!(hub.options.alpn, ALPN::H1);
        assert_eq!(hub.options.max_h2_streams, 1);
    }

    #[test]
    fn plaintext_auto_stays_h1_and_explicit_http2_enables_h2c() {
        let automatic: crate::config::UpstreamConfig =
            serde_saphyr::from_str("address: 127.0.0.1:9000").unwrap();
        let automatic = prepare_upstream("auto", &automatic).unwrap();
        assert_eq!(automatic.peer.options.alpn, ALPN::H1);

        let h2c: crate::config::UpstreamConfig = serde_saphyr::from_str(
            "address: 127.0.0.1:9000\nprotocol: http2\nhttp2_max_concurrent_streams: 64",
        )
        .unwrap();
        let h2c = prepare_upstream("h2c", &h2c).unwrap();
        assert_eq!(h2c.peer.options.alpn, ALPN::H2);
        assert_eq!(h2c.peer.options.max_h2_streams, 64);
    }

    #[test]
    fn forwarded_for_chain_limit_does_not_allocate_or_accept_oversized_chains() {
        let runtime = runtime();
        let peer = "127.0.0.1".parse().unwrap();
        let chain = std::iter::repeat_n("10.0.0.1", 33)
            .collect::<Vec<_>>()
            .join(",");
        assert_eq!(resolve_client_ip(&runtime, peer, Some(&chain)), peer);
        assert_eq!(
            resolve_client_ip(&runtime, peer, Some("invalid, 10.0.0.1")),
            peer
        );
    }

    #[test]
    fn forwarded_port_uses_the_actual_listener_with_safe_defaults() {
        assert_eq!(forwarded_port_value(Some(18_443), true).unwrap(), "18443");
        assert_eq!(forwarded_port_value(Some(18_080), false).unwrap(), "18080");
        assert_eq!(forwarded_port_value(None, true).unwrap(), "443");
        assert_eq!(forwarded_port_value(None, false).unwrap(), "80");
    }
}
