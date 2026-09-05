use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::net::{IpAddr, Ipv4Addr, ToSocketAddrs};
use std::sync::{Arc, Once};
use std::time::{Duration, Instant};

use ahash::AHashMap;
use anyhow::{Context, anyhow};
use arrayvec::ArrayString;
use bytes::Bytes;
use cloudflare_pingora::Error;
use cloudflare_pingora::ErrorType;
use cloudflare_pingora::ErrorType::HTTPStatus;
use cloudflare_pingora::Result;
use cloudflare_pingora::http::{RequestHeader, ResponseHeader};
use cloudflare_pingora::modules::http::HttpModules;
use cloudflare_pingora::modules::http::compression::{
    ResponseCompression, ResponseCompressionBuilder,
};
use cloudflare_pingora::prelude::HttpPeer;
use cloudflare_pingora::protocols::http::bridge::grpc_web::GrpcWebCtx;
use cloudflare_pingora::protocols::tls::CustomALPN;
use cloudflare_pingora::protocols::{ALPN, Digest, TcpKeepalive};
use cloudflare_pingora::proxy::{
    CacheMeta, FailToProxy, ForcedFreshness, HitHandler, ProxyHttp, RawSocketHandle, Session,
    default_fail_to_proxy,
};
use http::header::{
    ACCEPT_ENCODING, CONNECTION, CONTENT_LENGTH, FORWARDED, HOST, HeaderName, HeaderValue,
    STRICT_TRANSPORT_SECURITY, TE, TRANSFER_ENCODING, UPGRADE,
};
use http::{Method, Version};
use log::{debug, info, warn};
use serde_json::json;
use tokio::sync::mpsc;

use crate::config::{HandlerKind, RuntimeConfig, UpstreamProtocol, normalized_host};
use crate::content_encoding::{ContentCoding, EncodingNegotiation};
use crate::h3_wire;
use crate::handlers::adguard::{
    response_allows_compression, response_status_has_no_body, response_status_is_interim,
    strip_doh_caching_headers,
};
use crate::handlers::compression::{
    configure_downstream_compression, forwards_accept_encoding, uses_downstream_compression,
};
use crate::handlers::grpc;
use crate::kernel_socket::{self, PROXY_TCP_RCVBUF};
use crate::limits::{
    ActiveRequestLimiter, ActiveRequestPermit, GlobalConcurrentLimiter, GlobalConcurrentPermit,
    LimitZone, RateLimiter,
};
use crate::routing::{
    NAVIDROME_GRPC_UPSTREAM, RouteClass, classify_route, default_active_limit,
    upstream_name_for_route,
};
use crate::static_files::StaticFiles;
use crate::upstream_h3::{H3_UPSTREAM_ALPN, H3Route, UpstreamH3Registry};
static LEGACY_HEALTH_WARNING: Once = Once::new();
thread_local! {
    // A keep-alive or H2 connection normally sends many requests for the same
    // client. Keep only the most recently formatted address per worker thread
    // so the forwarded-header hot path avoids repeating integer formatting and
    // HeaderValue validation. The single entry is bounded and cannot grow with
    // attacker-controlled X-Forwarded-For values.
    static CLIENT_IP_HEADER_CACHE: RefCell<Option<(IpAddr, HeaderValue)>> = const {
        RefCell::new(None)
    };
    // Standard ports use static HeaderValues below. Cache the last nonstandard
    // listener port so development, sidecar, and benchmark listeners do not
    // repeat decimal formatting and validation on every keep-alive request.
    static FORWARDED_PORT_HEADER_CACHE: RefCell<Option<(u16, HeaderValue)>> = const {
        RefCell::new(None)
    };
}
const NO_PLAN: usize = usize::MAX;
const REQUEST_BODY_IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const MIN_REQUEST_BODY_RATE: usize = 64 * 1024;
const MAX_REQUEST_BODY_LIFETIME: Duration = Duration::from_secs(60 * 60);
const X_FORWARDED_FOR: HeaderName = HeaderName::from_static("x-forwarded-for");
const X_REAL_IP: HeaderName = HeaderName::from_static("x-real-ip");
const X_FORWARDED_HOST: HeaderName = HeaderName::from_static("x-forwarded-host");
const X_FORWARDED_PORT: HeaderName = HeaderName::from_static("x-forwarded-port");
const X_FORWARDED_PROTO: HeaderName = HeaderName::from_static("x-forwarded-proto");
const X_FORWARDED_SSL: HeaderName = HeaderName::from_static("x-forwarded-ssl");
const ALT_SVC: HeaderName = HeaderName::from_static("alt-svc");
const X_CONTENT_TYPE_OPTIONS: HeaderName = HeaderName::from_static("x-content-type-options");
const X_FRAME_OPTIONS: HeaderName = HeaderName::from_static("x-frame-options");
const REFERRER_POLICY: HeaderName = HeaderName::from_static("referrer-policy");
#[cfg(test)]
const KEEP_ALIVE: HeaderName = HeaderName::from_static("keep-alive");
const PROXY_CONNECTION: HeaderName = HeaderName::from_static("proxy-connection");
const MAX_CONNECTION_NOMINATIONS: usize = 10;
#[cfg(test)]
const PROXY_AUTHENTICATE: HeaderName = HeaderName::from_static("proxy-authenticate");
#[cfg(test)]
const PROXY_AUTHORIZATION: HeaderName = HeaderName::from_static("proxy-authorization");
const DIRECT_DOH_HOST: HeaderValue = HeaderValue::from_static("direct.tae00217.cloud");
const PORT_443: HeaderValue = HeaderValue::from_static("443");
const PORT_80: HeaderValue = HeaderValue::from_static("80");
const HTTPS: HeaderValue = HeaderValue::from_static("https");
const HTTP: HeaderValue = HeaderValue::from_static("http");
const ON: HeaderValue = HeaderValue::from_static("on");
const OFF: HeaderValue = HeaderValue::from_static("off");
const UPGRADE_VALUE: HeaderValue = HeaderValue::from_static("upgrade");
const TE_TRAILERS: HeaderValue = HeaderValue::from_static("trailers");
const NOSNIFF: HeaderValue = HeaderValue::from_static("nosniff");
const HSTS_VALUE: HeaderValue =
    HeaderValue::from_static("max-age=63072000; includeSubDomains; preload");
const SAMEORIGIN: HeaderValue = HeaderValue::from_static("SAMEORIGIN");
const REFERRER_POLICY_VALUE: HeaderValue =
    HeaderValue::from_static("strict-origin-when-cross-origin");

struct PreparedRouting {
    hosts: AHashMap<Arc<str>, PreparedHost>,
    plans: Box<[PreparedPlan]>,
    compression_modules: HttpModules,
}

#[derive(Clone, Debug)]
struct PreparedH3Peer {
    peer: HttpPeer,
    route: H3Route,
}

#[derive(Clone, Debug)]
struct PreparedUpstream {
    peer: HttpPeer,
    h3: Option<PreparedH3Peer>,
    read_timeout_seconds: Option<u64>,
    write_timeout_seconds: Option<u64>,
}

#[derive(Clone, Debug)]
struct PreparedPlan {
    upstream_host: HeaderValue,
    handler: HandlerKind,
    peer: HttpPeer,
    h3: Option<PreparedH3Peer>,
    route: RouteClass,
    rate_limit: Option<(f64, u32)>,
    active_request_limit: usize,
    downstream_timeout: Duration,
    max_body_bytes: usize,
}

#[derive(Debug)]
struct PreparedHost {
    domain: Arc<str>,
    name: String,
    handler: HandlerKind,
    redirect_http: bool,
    plans: [Option<usize>; RouteClass::ALL.len()],
}

impl PreparedHost {
    fn plan(&self, path: &str) -> Option<usize> {
        classify_route(self.handler, path).and_then(|route| self.plans[route.index()])
    }
}

pub struct RequestContext {
    plan_index: usize,
    client_ip: IpAddr,
    tls: bool,
    http3: bool,
    forwarded_port: Option<u16>,
    upstream_forwarded_for: Option<HeaderValue>,
    upstream_forwarded_port: Option<HeaderValue>,
    body_bytes: usize,
    body_deadline: Option<Instant>,
    retries: usize,
    identity_acceptable: bool,
    compression_selected: bool,
    grpc: Option<grpc::GrpcKind>,
    grpc_web: GrpcWebCtx,
    started_at: Option<Instant>,
    upstream_h3_tcp_fallback: bool,
    _active_request_permit: Option<ActiveRequestPermit>,
    _global_request_permit: Option<GlobalConcurrentPermit>,
}

impl Default for RequestContext {
    fn default() -> Self {
        Self {
            plan_index: NO_PLAN,
            client_ip: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            tls: false,
            http3: false,
            forwarded_port: None,
            upstream_forwarded_for: None,
            upstream_forwarded_port: None,
            body_bytes: 0,
            body_deadline: None,
            retries: 0,
            identity_acceptable: true,
            compression_selected: false,
            grpc: None,
            grpc_web: GrpcWebCtx::Disabled,
            started_at: None,
            upstream_h3_tcp_fallback: false,
            _active_request_permit: None,
            _global_request_permit: None,
        }
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum Http3AdmissionRejection {
    RateLimited,
    TooManyConnections,
}

/// Process-wide admission and static cache shared by the public listener
/// and the HTTP/3 frontend. Separate Gateway instances must not own their
/// own limiters or LRU, or H2+H3 clients would get 2× quota and the host
/// would hold duplicate static asset caches.
pub struct GatewayShared {
    static_files: StaticFiles,
    rates: RateLimiter,
    active_requests: ActiveRequestLimiter,
    global_concurrent: GlobalConcurrentLimiter,
}

impl GatewayShared {
    pub fn admit_http3_connection(
        &self,
        peer: std::net::SocketAddr,
        rate_per_second: f64,
        burst: u32,
        max_active: usize,
    ) -> Result<ActiveRequestPermit, Http3AdmissionRejection> {
        if !self.rates.allow(
            LimitZone::Http3Connection,
            peer.ip(),
            rate_per_second,
            burst,
        ) {
            return Err(Http3AdmissionRejection::RateLimited);
        }
        self.active_requests
            .acquire(LimitZone::Http3Connection, peer.ip(), max_active)
            .ok_or(Http3AdmissionRejection::TooManyConnections)
    }

    pub fn from_runtime(runtime: &RuntimeConfig) -> anyhow::Result<Self> {
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
        Ok(Self {
            static_files: StaticFiles::new(roots, runtime.config.server.static_cache_bytes)?,
            rates: RateLimiter::new(),
            active_requests: ActiveRequestLimiter::new(),
            global_concurrent: GlobalConcurrentLimiter::new(),
        })
    }
}

pub struct Gateway {
    runtime: Arc<RuntimeConfig>,
    upstream_h3: Arc<UpstreamH3Registry>,
    shared: Arc<GatewayShared>,
    routing: Arc<PreparedRouting>,
}

impl Clone for Gateway {
    fn clone(&self) -> Self {
        Self {
            runtime: self.runtime.clone(),
            upstream_h3: self.upstream_h3.clone(),
            shared: self.shared.clone(),
            routing: self.routing.clone(),
        }
    }
}

impl Gateway {
    #[cfg(test)]
    pub fn new(
        runtime: Arc<RuntimeConfig>,
        upstream_h3: Arc<UpstreamH3Registry>,
    ) -> anyhow::Result<Self> {
        let shared = GatewayShared::from_runtime(&runtime)?;
        Self::with_shared(runtime, upstream_h3, Arc::new(shared))
    }

    pub fn with_shared(
        runtime: Arc<RuntimeConfig>,
        upstream_h3: Arc<UpstreamH3Registry>,
        shared: Arc<GatewayShared>,
    ) -> anyhow::Result<Self> {
        let upstreams = runtime
            .config
            .upstreams
            .iter()
            .map(|(name, upstream)| {
                prepare_upstream(name, upstream, &upstream_h3)
                    .map(|prepared| (name.clone(), prepared))
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
        let mut prepared_plans = Vec::new();
        for (name, host) in &runtime.config.hosts {
            for domain in &host.domains {
                let canonical_domain: Arc<str> = Arc::from(domain.as_str());
                let domain_header = http::HeaderValue::from_str(domain).with_context(|| {
                    format!(
                        "host domain cannot be encoded as a header: host={name} domain={domain}"
                    )
                })?;
                let plans = RouteClass::ALL.map(|route| {
                    // Navidrome gRPC prefers the dedicated plaintext-H2C
                    // upstream (WireGuard overlay, no second TLS layer).
                    // Falls back to the handler's configured upstream when it
                    // is not defined, so older configs keep working over
                    // TLS H2/H3 against Navidrome's main listener.
                    let dedicated_grpc = route == RouteClass::NavidromeGrpc
                        && matches!(
                            host.handler,
                            HandlerKind::NavidromeMain | HandlerKind::NavidromeCdn
                        )
                        && upstreams.contains_key(NAVIDROME_GRPC_UPSTREAM);
                    let upstream_name = if dedicated_grpc {
                        NAVIDROME_GRPC_UPSTREAM
                    } else {
                        upstream_name_for_route(host.handler, host.upstream.as_deref(), route)?
                    };
                    let upstream = upstreams.get(upstream_name)?;
                    let plan_index = prepared_plans.len();
                    prepared_plans.push(PreparedPlan {
                        upstream_host: if route == RouteClass::Doh {
                            DIRECT_DOH_HOST.clone()
                        } else {
                            domain_header.clone()
                        },
                        handler: host.handler,
                        peer: prepare_route_peer(upstream, route),
                        h3: prepare_route_h3(upstream, route),
                        route,
                        rate_limit: effective_rate_limit(&runtime, route),
                        active_request_limit: runtime
                            .config
                            .route_limits
                            .get(route.name())
                            .and_then(|limit| limit.active_requests)
                            .unwrap_or_else(|| default_active_limit(host.handler, route)),
                        downstream_timeout: Duration::from_secs(route.timeout_seconds()),
                        max_body_bytes: host.max_body_bytes,
                    });
                    Some(plan_index)
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
        Ok(Self {
            runtime,
            upstream_h3,
            shared,
            routing: Arc::new(PreparedRouting {
                hosts,
                plans: prepared_plans.into_boxed_slice(),
                compression_modules: {
                    let mut modules = HttpModules::new();
                    modules.add_module(ResponseCompressionBuilder::enable(1));
                    modules
                },
            }),
        })
    }

    fn host(&self, authority: &str) -> Option<&PreparedHost> {
        if let Some(host) = self.routing.hosts.get(authority) {
            return Some(host);
        }
        let domain = normalized_host(authority);
        match domain {
            std::borrow::Cow::Borrowed(same) if same == authority => None,
            other => self.routing.hosts.get::<str>(other.as_ref()),
        }
    }

    fn acquire_global_request(&self, ctx: &mut RequestContext) -> bool {
        let limit = self.runtime.config.server.global_active_requests;
        if limit == 0 {
            return true;
        }
        let Some(permit) = self.shared.global_concurrent.acquire(limit) else {
            return false;
        };
        ctx._global_request_permit = Some(permit);
        true
    }

    fn request_plan(&self, ctx: &RequestContext) -> Result<&PreparedPlan> {
        self.routing
            .plans
            .get(ctx.plan_index)
            .ok_or_else(|| Error::explain(HTTPStatus(500), "request plan is missing"))
    }

    fn prepare_upstream_forwarded_headers(
        &self,
        session: &Session,
        ctx: &mut RequestContext,
    ) -> Result<()> {
        ctx.upstream_forwarded_for = Some(forwarded_client_ip_value(ctx.client_ip)?);
        let listener_port = ctx.forwarded_port.or_else(|| {
            session
                .server_addr()
                .and_then(|address| address.as_inet())
                .map(|address| address.port())
        });
        ctx.upstream_forwarded_port = Some(forwarded_port_value(listener_port, ctx.tls)?);
        Ok(())
    }

    fn is_benign_stream_disconnect(
        &self,
        session: &Session,
        ctx: &RequestContext,
        error: &Error,
    ) -> bool {
        if !ctx.http3 && !is_direct_http3(session) {
            return false;
        }
        if !matches!(error.etype(), ErrorType::WriteError | ErrorType::ReadError) {
            return false;
        }
        let message = error.to_string();
        if !message.contains("response stream closed")
            && !message.contains("response write timed out")
            && !message.contains("downstream error while idling")
            && !message.contains("stream closed because of a broken pipe")
            && !message.contains("inactive stream")
        {
            return false;
        }
        session.response_written().is_some()
            || self.routing.plans.get(ctx.plan_index).is_some_and(|plan| {
                matches!(
                    plan.route,
                    RouteClass::NavidromeStream | RouteClass::NavidromeCover
                )
            })
    }
}

impl ProxyHttp for Gateway {
    type CTX = RequestContext;

    fn init_downstream_modules(&self, _modules: &mut HttpModules) {
        // Most routes deliberately use identity encoding or let Navidrome
        // negotiate with its origin. Install the bounded response compressor
        // lazily only for requests that actually selected a coding, avoiding a
        // boxed module and its async filter futures on the common identity path.
    }

    fn new_ctx(&self) -> Self::CTX {
        RequestContext {
            started_at: self.runtime.config.server.access_log.then(Instant::now),
            ..RequestContext::default()
        }
    }

    async fn early_request_filter(
        &self,
        _session: &mut Session,
        _ctx: &mut Self::CTX,
    ) -> Result<()> {
        Ok(())
    }

    async fn cache_hit_filter(
        &self,
        _session: &mut Session,
        _meta: &CacheMeta,
        _hit_handler: &mut HitHandler,
        _is_fresh: bool,
        _ctx: &mut Self::CTX,
    ) -> Result<Option<ForcedFreshness>> {
        Ok(None)
    }

    async fn proxy_upstream_filter(
        &self,
        _session: &mut Session,
        _ctx: &mut Self::CTX,
    ) -> Result<bool> {
        Ok(true)
    }

    async fn request_filter(&self, session: &mut Session, ctx: &mut Self::CTX) -> Result<bool> {
        let direct_h3 = is_direct_http3(session);
        let plain_h1 = session.downstream_session.as_http1().is_some() && !is_tls(session);
        let http3 = direct_h3;
        let tls = if plain_h1 {
            false
        } else {
            is_tls(session) || http3
        };
        ctx.http3 = http3;
        ctx.tls = tls;
        ctx.forwarded_port = if direct_h3 {
            self.runtime.http3_public_port()
        } else {
            None
        };
        if request_target_has_forbidden_bytes(session.req_header()) {
            session.set_keepalive(None);
            return send_empty(&self.runtime, session, 400, None, tls, http3, &[]).await;
        }
        let declared_content_length = match validated_request_content_length(session.req_header()) {
            Ok(length) => length,
            Err(()) => {
                session.set_keepalive(None);
                return send_empty(&self.runtime, session, 400, None, tls, http3, &[]).await;
            }
        };
        let path = session.req_header().uri.path().to_owned();

        if path == "/pingora-health" || path == "/pingora-live" || path == "/pingora-ready" {
            return send_empty(
                &self.runtime,
                session,
                204,
                None,
                tls,
                http3,
                &[("x-proxy-product", "Pingora")],
            )
            .await;
        }
        if path == "/pingola-health" {
            if self.runtime.config.server.legacy_pingola_health {
                LEGACY_HEALTH_WARNING.call_once(|| {
                    warn!(
                        "/pingola-health is deprecated; migrate to /pingora-health (legacy support will be removed after one release)"
                    );
                });
                return send_empty(
                    &self.runtime,
                    session,
                    204,
                    None,
                    tls,
                    http3,
                    &[("x-proxy-product", "Pingora"), ("deprecation", "true")],
                )
                .await;
            }
            return send_empty(
                &self.runtime,
                session,
                404,
                None,
                tls,
                http3,
                &[("x-proxy-product", "Pingora")],
            )
            .await;
        }
        if path == "/nginx-health" {
            return send_empty(
                &self.runtime,
                session,
                404,
                None,
                tls,
                http3,
                &[("x-proxy-product", "Pingora")],
            )
            .await;
        }
        if path == "/pingora-health/details" {
            let unix_socket = session
                .client_addr()
                .and_then(|address| address.as_inet())
                .is_none();
            if !self.runtime.config.server.health_details || !unix_socket {
                return send_empty(
                    &self.runtime,
                    session,
                    404,
                    None,
                    tls,
                    http3,
                    &[("x-proxy-product", "Pingora")],
                )
                .await;
            }
            return send_health_details(session, &self.runtime, &self.upstream_h3).await;
        }

        let Some(authority) = request_authority(session.req_header()) else {
            return send_empty(&self.runtime, session, 400, None, tls, http3, &[]).await;
        };
        let Some(host) = self.host(authority) else {
            session.set_keepalive(None);
            return send_empty(&self.runtime, session, 421, None, tls, http3, &[]).await;
        };

        if !tls && host.redirect_http {
            let path_and_query = session
                .req_header()
                .uri
                .path_and_query()
                .map_or("/", |value| value.as_str());
            let location = format!("https://{}{path_and_query}", host.domain.as_ref());
            return send_empty(
                &self.runtime,
                session,
                308,
                Some(host.handler),
                false,
                http3,
                &[("location", location.as_str())],
            )
            .await;
        }

        if host.handler == HandlerKind::Static {
            // Bound slow readers while a cold-asset memory permit is held.
            session.set_read_timeout(Some(Duration::from_secs(30)));
            session.set_write_timeout(Some(Duration::from_secs(30)));
            session.set_keepalive(Some(30));
            let Some(client_ip) = session_client_ip(&self.runtime, session) else {
                session.set_keepalive(None);
                return send_empty(&self.runtime, session, 400, None, tls, http3, &[]).await;
            };
            ctx.client_ip = client_ip;
            if !self.acquire_global_request(ctx) {
                return send_empty(
                    &self.runtime,
                    session,
                    429,
                    Some(host.handler),
                    tls,
                    http3,
                    &[("retry-after", "1")],
                )
                .await;
            }
            if let Some(permit) = self.shared.active_requests.acquire(
                LimitZone::Static,
                client_ip,
                self.runtime.config.server.static_active_requests_per_client,
            ) {
                ctx._active_request_permit = Some(permit);
            } else {
                return send_empty(
                    &self.runtime,
                    session,
                    429,
                    Some(host.handler),
                    tls,
                    http3,
                    &[("retry-after", "1")],
                )
                .await;
            }
            return self
                .shared
                .static_files
                .serve(&host.name, session, tls)
                .await;
        }

        let Some(client_ip) = session_client_ip(&self.runtime, session) else {
            session.set_keepalive(None);
            return send_empty(&self.runtime, session, 400, None, tls, http3, &[]).await;
        };
        ctx.client_ip = client_ip;
        self.prepare_upstream_forwarded_headers(session, ctx)?;

        if host.handler == HandlerKind::NavidromeMain && path == "/" {
            let location = format!("https://{}/app/", host.domain.as_ref());
            return send_empty(
                &self.runtime,
                session,
                308,
                Some(host.handler),
                tls,
                http3,
                &[("location", location.as_str())],
            )
            .await;
        }

        let grpc_kind = grpc::classify_request(session.req_header());
        ctx.grpc = grpc_kind;
        let Some(plan_index) = ({
            // application/grpc* to Navidrome rides the dedicated plaintext-H2C
            // plan (prior-knowledge H2, no TLS under WireGuard). Everything
            // else keeps the existing path-based plan; when the dedicated
            // upstream is not configured the gRPC plan already falls back to
            // the handler's upstream, and path routing is the final fallback.
            let grpc_to_navidrome = grpc_kind.is_some()
                && matches!(
                    host.handler,
                    HandlerKind::NavidromeMain | HandlerKind::NavidromeCdn
                );
            if grpc_to_navidrome {
                host.plans[RouteClass::NavidromeGrpc.index()].or_else(|| host.plan(&path))
            } else {
                host.plan(&path)
            }
        }) else {
            return send_empty(
                &self.runtime,
                session,
                500,
                Some(host.handler),
                tls,
                http3,
                &[],
            )
            .await;
        };
        let plan = &self.routing.plans[plan_index];
        let encoding = if uses_downstream_compression(plan.route) && grpc_kind.is_none() {
            configure_downstream_compression(
                session,
                plan.route,
                &self.routing.compression_modules,
            )?
        } else {
            EncodingNegotiation {
                preferred: ContentCoding::Identity,
                identity_acceptable: true,
            }
        };
        if encoding.preferred == ContentCoding::NotAcceptable {
            return send_empty(
                &self.runtime,
                session,
                406,
                Some(plan.handler),
                tls,
                http3,
                &[],
            )
            .await;
        }
        ctx.identity_acceptable = encoding.identity_acceptable;
        ctx.compression_selected = encoding.preferred.as_str().is_some();

        if declared_content_length.is_some_and(|length| length > plan.max_body_bytes) {
            return send_empty(
                &self.runtime,
                session,
                413,
                Some(plan.handler),
                tls,
                http3,
                &[],
            )
            .await;
        }

        if let Some((rate, burst)) = plan.rate_limit
            && !self
                .shared
                .rates
                .allow(plan.route.limit_zone(), client_ip, rate, burst)
        {
            return send_empty(
                &self.runtime,
                session,
                429,
                Some(plan.handler),
                tls,
                http3,
                &[("retry-after", "1")],
            )
            .await;
        }

        if !self.acquire_global_request(ctx) {
            return send_empty(
                &self.runtime,
                session,
                429,
                Some(plan.handler),
                tls,
                http3,
                &[("retry-after", "1")],
            )
            .await;
        }

        if plan.active_request_limit > 0 {
            let Some(permit) = self.shared.active_requests.acquire(
                plan.route.limit_zone(),
                client_ip,
                plan.active_request_limit,
            ) else {
                return send_empty(
                    &self.runtime,
                    session,
                    429,
                    Some(plan.handler),
                    tls,
                    http3,
                    &[("retry-after", "1")],
                )
                .await;
            };
            ctx._active_request_permit = Some(permit);
        }

        let has_request_body = declared_content_length.is_some_and(|length| length > 0)
            || session.req_header().headers.contains_key(TRANSFER_ENCODING)
            || !matches!(session.req_header().method, Method::GET | Method::HEAD);
        if has_request_body {
            session.set_read_timeout(Some(REQUEST_BODY_IDLE_TIMEOUT));
            ctx.body_deadline = Some(Instant::now() + request_body_lifetime(plan.max_body_bytes));
        } else {
            session.set_read_timeout(Some(plan.downstream_timeout));
        }
        session.set_write_timeout(Some(plan.downstream_timeout));
        session.set_keepalive(Some(30));
        ctx.plan_index = plan_index;
        debug!(
            "integration route host={} path={} route={} handler={:?} upstream_pool_group={} h3_eligible={}",
            host.name,
            path,
            plan.route.name(),
            plan.handler,
            plan.route.upstream_pool_group(),
            plan.h3.is_some(),
        );

        Ok(false)
    }

    async fn upstream_peer(
        &self,
        _session: &mut Session,
        ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>> {
        let plan = self.request_plan(ctx)?;
        if ctx.grpc.is_none()
            && let Some(h3) = &plan.h3
            && h3.route.should_use_direct_h3(ctx.upstream_h3_tcp_fallback)
        {
            return Ok(Box::new(h3.peer.clone()));
        }
        Ok(Box::new(plan.peer.clone()))
    }

    fn precomputed_upstream_peer<'a>(&'a self, ctx: &Self::CTX) -> Option<&'a HttpPeer> {
        let plan = self.routing.plans.get(ctx.plan_index)?;
        if ctx.grpc.is_some() {
            return Some(&plan.peer);
        }
        match plan.h3.as_ref() {
            Some(h3) if h3.route.should_use_direct_h3(ctx.upstream_h3_tcp_fallback) => {
                Some(&h3.peer)
            }
            _ => Some(&plan.peer),
        }
    }

    fn h1_bodyless_fast_path(&self, session: &Session, ctx: &Self::CTX) -> bool {
        let Some(plan) = self.routing.plans.get(ctx.plan_index) else {
            return false;
        };
        plan.route.supports_h1_bodyless_fast_path() && !session.is_upgrade_req()
    }

    fn sync_upstream_request_wire(
        &self,
        wire: &mut Vec<(Bytes, Bytes)>,
        upstream_request: &RequestHeader,
    ) {
        h3_wire::finalize_upstream_wire_pairs(wire, upstream_request);
    }

    fn h1_bodyless_poll_downstream(&self, _session: &Session, ctx: &Self::CTX) -> bool {
        self.routing.plans.get(ctx.plan_index).is_some_and(|plan| {
            matches!(
                plan.route,
                RouteClass::NavidromeStream | RouteClass::NavidromeCover
            )
        })
    }

    async fn upstream_request_filter(
        &self,
        session: &mut Session,
        upstream_request: &mut RequestHeader,
        ctx: &mut Self::CTX,
    ) -> Result<()> {
        let plan = self.request_plan(ctx)?;
        strip_request_hop_headers(session.req_header(), upstream_request)?;
        grpc::prepare_upstream_request(upstream_request, &mut ctx.grpc_web, ctx.grpc);
        upstream_request.headers.reserve(10);
        let client_ip = ctx.upstream_forwarded_for.as_ref().ok_or_else(|| {
            Error::explain(HTTPStatus(500), "upstream forwarded client IP is missing")
        })?;
        let forwarded_port = ctx
            .upstream_forwarded_port
            .as_ref()
            .ok_or_else(|| Error::explain(HTTPStatus(500), "upstream forwarded port is missing"))?;

        upstream_request.remove_header(&FORWARDED);
        upstream_request.remove_header(&X_FORWARDED_FOR);
        upstream_request.insert_typed_header(HOST, plan.upstream_host.clone());
        upstream_request.insert_typed_header(X_REAL_IP, client_ip.clone());
        upstream_request.insert_typed_header(X_FORWARDED_FOR, client_ip.clone());
        upstream_request.insert_typed_header(X_FORWARDED_HOST, plan.upstream_host.clone());
        upstream_request.insert_typed_header(X_FORWARDED_PORT, forwarded_port.clone());
        upstream_request.insert_typed_header(X_FORWARDED_PROTO, if ctx.tls { HTTPS } else { HTTP });
        upstream_request.insert_typed_header(X_FORWARDED_SSL, if ctx.tls { ON } else { OFF });

        if forwards_accept_encoding(plan.route) && ctx.grpc.is_none() {
            if let Some(value) = session.req_header().headers.get(ACCEPT_ENCODING) {
                upstream_request.insert_typed_header(ACCEPT_ENCODING, value.clone());
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
            upstream_request.insert_typed_header(UPGRADE, upgrade.clone());
            upstream_request.insert_typed_header(CONNECTION, UPGRADE_VALUE);
        }
        Ok(())
    }

    async fn upstream_response_filter(
        &self,
        _session: &mut Session,
        _upstream_response: &mut ResponseHeader,
        _ctx: &mut Self::CTX,
    ) -> Result<()> {
        Ok(())
    }

    async fn custom_forwarding(
        &self,
        _session: &mut Session,
        _ctx: &mut Self::CTX,
        _custom_message_to_upstream: Option<mpsc::Sender<Bytes>>,
        _custom_message_to_downstream: mpsc::Sender<Bytes>,
    ) -> Result<()> {
        Ok(())
    }

    async fn downstream_custom_message_proxy_filter(
        &self,
        _session: &mut Session,
        custom_message: Bytes,
        _ctx: &mut Self::CTX,
        _final_hop: bool,
    ) -> Result<Option<Bytes>> {
        Ok(Some(custom_message))
    }

    async fn upstream_custom_message_proxy_filter(
        &self,
        _session: &mut Session,
        custom_message: Bytes,
        _ctx: &mut Self::CTX,
        _final_hop: bool,
    ) -> Result<Option<Bytes>> {
        Ok(Some(custom_message))
    }

    async fn request_body_filter(
        &self,
        session: &mut Session,
        body: &mut Option<Bytes>,
        end_of_stream: bool,
        ctx: &mut Self::CTX,
    ) -> Result<()> {
        if ctx
            .body_deadline
            .is_some_and(|deadline| Instant::now() > deadline)
        {
            session.set_keepalive(None);
            return Err(Error::explain(
                HTTPStatus(408),
                "request body upload deadline exceeded",
            ));
        }
        ctx.body_bytes = ctx
            .body_bytes
            .saturating_add(body.as_ref().map_or(0, Bytes::len));
        if self
            .routing
            .plans
            .get(ctx.plan_index)
            .is_some_and(|plan| ctx.body_bytes > plan.max_body_bytes)
        {
            return Err(Error::explain(HTTPStatus(413), "request body is too large"));
        }
        if end_of_stream {
            ctx.body_deadline = None;
            if let Some(plan) = self.routing.plans.get(ctx.plan_index) {
                session.set_read_timeout(Some(plan.downstream_timeout));
            }
        }
        Ok(())
    }

    async fn response_filter(
        &self,
        session: &mut Session,
        response: &mut ResponseHeader,
        ctx: &mut Self::CTX,
    ) -> Result<()> {
        let Some(plan) = self.routing.plans.get(ctx.plan_index) else {
            return Ok(());
        };
        let forwards_upgrade = response.status.as_u16() == 101
            && session.req_header().version == Version::HTTP_11
            && session.is_upgrade_req();
        strip_response_hop_headers(response, forwards_upgrade)?;
        grpc::apply_web_response(&mut ctx.grpc_web, response);
        if self.runtime.config.server.security_headers {
            insert_security_headers(response, plan.handler, ctx.tls)?;
        }
        if ctx.tls
            && !ctx.http3
            && let Some(alt_svc) = self.runtime.http3_alt_svc_header()
        {
            response.insert_typed_header(ALT_SVC, alt_svc.clone());
        }
        let bodyless = response_status_has_no_body(response.status.as_u16());
        if ctx.compression_selected && response_status_is_interim(response.status.as_u16()) {
            // 100/103 are interim headers. Do not permanently disable the
            // compressor before the final response arrives.
            return Ok(());
        }
        // Status-defined no-content responses carry no selected representation.
        // HEAD still describes the corresponding GET representation, so it
        // must follow the same content-coding acceptability decision as GET.
        if ctx.compression_selected && (bodyless || !response_allows_compression(response)) {
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
            strip_doh_caching_headers(response);
        }
        Ok(())
    }

    async fn response_trailer_filter(
        &self,
        _session: &mut Session,
        upstream_trailers: &mut http::HeaderMap,
        ctx: &mut Self::CTX,
    ) -> Result<Option<Bytes>> {
        ctx.grpc_web.response_trailer_filter(upstream_trailers)
    }

    async fn fail_to_proxy(
        &self,
        session: &mut Session,
        error: &Error,
        ctx: &mut Self::CTX,
    ) -> FailToProxy {
        if self.is_benign_stream_disconnect(session, ctx, error) {
            return FailToProxy {
                error_code: 0,
                can_reuse_downstream: true,
            };
        }
        default_fail_to_proxy(session, error).await
    }

    fn suppress_error_log(&self, session: &Session, ctx: &Self::CTX, error: &Error) -> bool {
        self.is_benign_stream_disconnect(session, ctx, error)
    }

    async fn connected_to_upstream(
        &self,
        _session: &mut Session,
        _reused: bool,
        _peer: &HttpPeer,
        _socket: RawSocketHandle,
        _digest: Option<&Digest>,
        _ctx: &mut Self::CTX,
    ) -> Result<()> {
        Ok(())
    }

    fn fail_to_connect(
        &self,
        session: &mut Session,
        peer: &HttpPeer,
        ctx: &mut Self::CTX,
        mut error: Box<Error>,
    ) -> Box<Error> {
        if try_upstream_h3_tcp_fallback(self, session, peer, ctx, &mut error) {
            return error;
        }

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
        if self.is_benign_stream_disconnect(session, ctx, &error) {
            error.set_retry(false);
            return error;
        }
        if session.response_written().is_some() {
            error.set_retry(false);
            return error;
        }
        if try_upstream_h3_tcp_fallback(self, session, peer, ctx, &mut error) {
            return error;
        }
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
        if error.is_none() && !self.runtime.config.server.access_log {
            return;
        }
        let status = session
            .response_written()
            .map_or(0, |response| response.status.as_u16());
        if let Some(error) = error {
            if self.is_benign_stream_disconnect(session, ctx, error) {
                debug!(
                    "stream disconnect client={} status={} error={}",
                    ctx.client_ip, status, error
                );
            } else if let Some(started_at) = ctx.started_at {
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
                session.req_header().uri.path(),
                status,
                elapsed.as_millis()
            );
        }
    }
}

fn connection_option_names(
    headers: &http::HeaderMap,
    invalid_status: u16,
) -> Result<arrayvec::ArrayVec<HeaderName, MAX_CONNECTION_NOMINATIONS>> {
    let mut names = arrayvec::ArrayVec::new();
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
                if names.len() == MAX_CONNECTION_NOMINATIONS {
                    return Err(Error::explain(
                        HTTPStatus(invalid_status),
                        "too many Connection header options",
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
    // Scan keys before Connection nominations can hide TE. Hub and ordinary
    // requests have no TE, so this branch is not taken on that path.
    let mut saw_te = false;
    let fixed: arrayvec::ArrayVec<HeaderName, 8> = upstream
        .headers
        .keys()
        .filter(|name| {
            if *name == TE {
                saw_te = true;
                true
            } else {
                is_fixed_request_hop_header(name)
            }
        })
        .cloned()
        .collect();
    let keep_te_trailers = saw_te && te_includes_trailers(&upstream.headers);
    // HTTP/3 requests do not carry HTTP/1-style Connection options.
    if downstream.headers.contains_key(CONNECTION)
        || downstream.headers.contains_key(PROXY_CONNECTION)
    {
        let mut nominations = 0;
        for field in [&CONNECTION, &PROXY_CONNECTION] {
            for value in downstream.headers.get_all(field).iter() {
                for token in value.as_bytes().split(|byte| *byte == b',') {
                    let token = token.trim_ascii();
                    if token.is_empty() {
                        continue;
                    }
                    nominations += 1;
                    if nominations > MAX_CONNECTION_NOMINATIONS {
                        return Err(Error::explain(
                            HTTPStatus(400),
                            "too many Connection header options",
                        ));
                    }
                    let name = HeaderName::from_bytes(token).map_err(|error| {
                        Error::because(HTTPStatus(400), "invalid Connection header option", error)
                    })?;
                    if name == CONTENT_LENGTH || name == TRANSFER_ENCODING || name == HOST {
                        return Err(Error::explain(
                            HTTPStatus(400),
                            format!("Connection header names critical framing field {name}"),
                        ));
                    }
                    upstream.remove_header(&name);
                }
            }
        }
    }
    // A small normal request usually has fewer headers than this fixed list.
    // One linear scan avoids eight absent-key hash lookups on every request.
    for name in fixed {
        upstream.remove_header(&name);
    }
    if keep_te_trailers {
        upstream.insert_typed_header(TE, TE_TRAILERS);
    }
    normalize_content_length_headers(&mut upstream.headers, 400)?;
    Ok(())
}

fn te_includes_trailers(headers: &http::HeaderMap) -> bool {
    headers.get_all(TE).iter().any(|value| {
        value
            .as_bytes()
            .split(|byte| *byte == b',')
            .any(|token| token.trim_ascii().eq_ignore_ascii_case(b"trailers"))
    })
}

fn strip_response_hop_headers(response: &mut ResponseHeader, forwards_upgrade: bool) -> Result<()> {
    let upgrade = forwards_upgrade
        .then(|| response.headers.get(UPGRADE).cloned())
        .flatten();
    let connection_options = connection_option_names(&response.headers, 502)?;
    // Hop-by-hop fields and upstream identity/security fields are both absent
    // from the downstream response. Filter them in one pass instead of doing
    // fifteen independent HeaderMap hash removals for every response.
    let fixed: arrayvec::ArrayVec<HeaderName, 15> = response
        .headers
        .keys()
        .filter(|name| is_fixed_response_removed_header(name))
        .cloned()
        .collect();
    for name in connection_options {
        response.remove_header(&name);
    }
    for name in fixed {
        response.remove_header(&name);
    }
    normalize_content_length_headers(&mut response.headers, 502)?;
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

fn is_fixed_request_hop_header(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "connection"
            | "keep-alive"
            | "proxy-connection"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "upgrade"
    )
}

fn is_fixed_response_removed_header(name: &HeaderName) -> bool {
    is_fixed_request_hop_header(name)
        || matches!(
            name.as_str(),
            "server"
                | "x-powered-by"
                | "alt-svc"
                | "strict-transport-security"
                | "x-content-type-options"
                | "x-frame-options"
                | "referrer-policy"
        )
}

fn request_authority(request: &RequestHeader) -> Option<&str> {
    let mut host_values = request.headers.get_all(HOST).iter();
    let host = host_values.next();
    if host_values.next().is_some() {
        return None;
    }
    host.and_then(|value| value.to_str().ok())
        .or_else(|| request.uri.authority().map(|value| value.as_str()))
}

fn request_target_has_forbidden_bytes(request: &RequestHeader) -> bool {
    request
        .uri
        .path_and_query()
        .map_or("/", |value| value.as_str())
        .as_bytes()
        .iter()
        .any(|byte| matches!(byte, 0x00 | b'\n' | b'\r' | b' '))
}

fn request_body_lifetime(max_body_bytes: usize) -> Duration {
    let transfer_seconds =
        max_body_bytes.saturating_add(MIN_REQUEST_BODY_RATE - 1) / MIN_REQUEST_BODY_RATE;
    Duration::from_secs(u64::try_from(transfer_seconds).unwrap_or(u64::MAX).max(60))
        .min(MAX_REQUEST_BODY_LIFETIME)
}

fn validated_content_length(headers: &http::HeaderMap) -> std::result::Result<Option<usize>, ()> {
    let mut expected = None;
    for value in headers.get_all(CONTENT_LENGTH).iter() {
        let value = value.to_str().map_err(|_| ())?;
        for token in value.split(',') {
            let token = token.trim_ascii();
            if token.is_empty() || !token.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err(());
            }
            let length = token.parse::<usize>().map_err(|_| ())?;
            if expected.is_some_and(|previous| previous != length) {
                return Err(());
            }
            expected = Some(length);
        }
    }
    Ok(expected)
}

fn validated_request_content_length(
    request: &RequestHeader,
) -> std::result::Result<Option<usize>, ()> {
    let length = validated_content_length(&request.headers)?;
    if request.headers.contains_key(TRANSFER_ENCODING)
        && (length.is_some() || !matches!(request.version, Version::HTTP_10 | Version::HTTP_11))
    {
        return Err(());
    }
    Ok(length)
}

fn normalize_content_length_headers(
    headers: &mut http::HeaderMap,
    invalid_status: u16,
) -> Result<()> {
    let length = validated_content_length(headers).map_err(|()| {
        Error::explain(
            HTTPStatus(invalid_status),
            "invalid or conflicting Content-Length fields",
        )
    })?;
    if headers.contains_key(TRANSFER_ENCODING) && length.is_some() {
        return Err(Error::explain(
            HTTPStatus(invalid_status),
            "message contains both Transfer-Encoding and Content-Length",
        ));
    }
    if let Some(length) = length {
        let already_canonical = {
            let mut values = headers.get_all(CONTENT_LENGTH).iter();
            values.next().is_some_and(|value| {
                !value.as_bytes().is_empty()
                    && value.as_bytes().iter().all(u8::is_ascii_digit)
                    && values.next().is_none()
            })
        };
        if already_canonical {
            return Ok(());
        }
        headers.remove(CONTENT_LENGTH);
        let mut encoded = ArrayString::<39>::new();
        write!(&mut encoded, "{length}").map_err(|error| {
            Error::because(
                HTTPStatus(invalid_status),
                "validated Content-Length could not be formatted",
                error,
            )
        })?;
        let value = HeaderValue::from_str(&encoded).map_err(|error| {
            Error::because(
                HTTPStatus(invalid_status),
                "validated Content-Length could not be encoded",
                error,
            )
        })?;
        headers.insert(CONTENT_LENGTH, value);
    }
    Ok(())
}

fn request_is_replay_safe(session: &mut Session) -> bool {
    request_header_is_replay_safe(session.req_header()) && session.as_mut().is_body_empty()
}

fn request_header_is_replay_safe(request: &RequestHeader) -> bool {
    matches!(request.method, Method::GET | Method::HEAD)
        && matches!(
            validated_request_content_length(request),
            Ok(None | Some(0))
        )
}

fn is_direct_http3(session: &Session) -> bool {
    // QUIC streams enter Gateway as a custom Pingora session, not via the
    // private loopback h2c listener.
    session.downstream_session.is_custom()
}

fn is_tls(session: &Session) -> bool {
    session
        .digest()
        .and_then(|digest| digest.ssl_digest.as_ref())
        .is_some()
}

fn try_upstream_h3_tcp_fallback(
    gateway: &Gateway,
    session: &mut Session,
    peer: &HttpPeer,
    ctx: &mut RequestContext,
    error: &mut Error,
) -> bool {
    if !matches!(peer.options.alpn, ALPN::Custom(_)) {
        return false;
    }
    let Ok(plan) = gateway.request_plan(ctx) else {
        return false;
    };
    let Some(h3) = &plan.h3 else {
        return false;
    };
    if !h3.route.allows_tcp_fallback()
        || ctx.upstream_h3_tcp_fallback
        || !request_is_replay_safe(session)
        || ctx.retries >= gateway.runtime.config.server.max_retries
    {
        return false;
    }
    ctx.upstream_h3_tcp_fallback = true;
    ctx.retries += 1;
    warn!(
        "upstream HTTP/3 direct path failed; falling back to TCP/TLS attempt={} method={} category={}",
        ctx.retries,
        session.req_header().method,
        error.etype().as_str(),
    );
    error.set_retry(true);
    true
}

fn session_client_ip(runtime: &RuntimeConfig, session: &Session) -> Option<IpAddr> {
    let peer_ip = session
        .client_addr()
        .and_then(|address| address.as_inet())
        .map_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED), |address| address.ip());
    if !runtime.is_trusted_proxy(peer_ip) {
        return Some(peer_ip);
    }
    let forwarded_for = match canonical_forwarded_for(&session.req_header().headers) {
        Ok(Some(value)) => value,
        Ok(None) => return Some(peer_ip),
        Err(()) => return None,
    };
    Some(resolve_client_ip(runtime, peer_ip, Some(forwarded_for)))
}

fn canonical_forwarded_for(headers: &http::HeaderMap) -> std::result::Result<Option<&str>, ()> {
    let mut values = headers.get_all(X_FORWARDED_FOR).iter();
    let Some(value) = values.next() else {
        return Ok(None);
    };
    if values.next().is_some() {
        return Err(());
    }
    value.to_str().map(Some).map_err(|_| ())
}

fn forwarded_port_value(port: Option<u16>, tls: bool) -> Result<HeaderValue> {
    match port {
        Some(80) => Ok(PORT_80),
        Some(443) => Ok(PORT_443),
        Some(port) => {
            if let Some(value) = FORWARDED_PORT_HEADER_CACHE.with(|cache| {
                cache
                    .borrow()
                    .as_ref()
                    .filter(|(cached_port, _)| *cached_port == port)
                    .map(|(_, value)| value.clone())
            }) {
                return Ok(value);
            }

            let mut value = ArrayString::<5>::new();
            write!(&mut value, "{port}").map_err(|error| {
                Error::because(
                    HTTPStatus(500),
                    "listener port could not be formatted as a header",
                    error,
                )
            })?;
            let value = HeaderValue::from_str(&value).map_err(|error| {
                Error::because(
                    HTTPStatus(500),
                    "listener port could not be encoded as a header",
                    error,
                )
            })?;
            FORWARDED_PORT_HEADER_CACHE.with(|cache| {
                *cache.borrow_mut() = Some((port, value.clone()));
            });
            Ok(value)
        }
        None if tls => Ok(PORT_443),
        None => Ok(PORT_80),
    }
}

fn forwarded_client_ip_value(ip: IpAddr) -> Result<HeaderValue> {
    if let Some(value) = CLIENT_IP_HEADER_CACHE.with(|cache| {
        cache
            .borrow()
            .as_ref()
            .filter(|(cached_ip, _)| *cached_ip == ip)
            .map(|(_, value)| value.clone())
    }) {
        return Ok(value);
    }

    let mut text = ArrayString::<64>::new();
    write!(&mut text, "{ip}").map_err(|error| {
        Error::because(
            HTTPStatus(400),
            "resolved client IP could not be formatted as a header",
            error,
        )
    })?;
    let value = HeaderValue::from_str(&text).map_err(|error| {
        Error::because(
            HTTPStatus(400),
            "resolved client IP could not be encoded as a header",
            error,
        )
    })?;
    CLIENT_IP_HEADER_CACHE.with(|cache| {
        *cache.borrow_mut() = Some((ip, value.clone()));
    });
    Ok(value)
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

fn prepare_route_h3(upstream: &PreparedUpstream, route: RouteClass) -> Option<PreparedH3Peer> {
    // gRPC requires HTTP/2 trailers (and the gRPC-web bridge). Navidrome's
    // dedicated gRPC listener is plaintext H2C; do not send those RPCs over
    // the music origin's HTTP/3 pool even when that upstream is preferred.
    if matches!(route, RouteClass::VaultwardenHub | RouteClass::NavidromeGrpc) {
        return None;
    }
    let mut h3 = upstream.h3.clone()?;
    h3.peer.group_key = 10_000 + route.upstream_pool_group();
    let (read_timeout, write_timeout) = upstream_timeouts(route, upstream);
    h3.peer.options.read_timeout = Some(read_timeout);
    h3.peer.options.write_timeout = Some(write_timeout);
    h3.peer.cache_reuse_hash();
    Some(h3)
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
    upstream_h3: &UpstreamH3Registry,
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
        UpstreamProtocol::Auto | UpstreamProtocol::Http3 | UpstreamProtocol::Http3Preferred
            if upstream.tls =>
        {
            ALPN::H2H1
        }
        UpstreamProtocol::Auto | UpstreamProtocol::Http1 => ALPN::H1,
        UpstreamProtocol::Http2 | UpstreamProtocol::Grpc => ALPN::H2,
        UpstreamProtocol::Http3 | UpstreamProtocol::Http3Preferred => ALPN::H1,
    };
    peer.options.max_h2_streams = upstream.http2_max_concurrent_streams;
    peer.options.h2_stream_window_size = Some(upstream.http2_stream_window_bytes);
    peer.options.h2_connection_window_size = Some(upstream.http2_connection_window_bytes);
    peer.options.h2_ping_interval = (upstream.http2_ping_interval_seconds > 0)
        .then_some(Duration::from_secs(upstream.http2_ping_interval_seconds));
    peer.options.tcp_keepalive = Some(TcpKeepalive {
        idle: Duration::from_secs(60),
        interval: Duration::from_secs(10),
        count: 3,
        #[cfg(target_os = "linux")]
        user_timeout: Duration::from_secs(90),
    });
    let offload = kernel_socket::offload_report();
    peer.options.tcp_fast_open = offload.tcp_fastopen_client;
    peer.options.tcp_recv_buf = Some(PROXY_TCP_RCVBUF);
    peer.options.upstream_tcp_sock_tweak_hook = Some(kernel_socket::upstream_tcp_hook());
    let h3 = upstream_h3.route(name).map(|route| {
        let mut peer = HttpPeer::new(address, false, name.to_string());
        peer.options.connection_timeout =
            Some(Duration::from_secs(upstream.connect_timeout_seconds));
        peer.options.total_connection_timeout =
            Some(Duration::from_secs(upstream.connect_timeout_seconds));
        peer.options.idle_timeout = Some(Duration::from_secs(upstream.idle_timeout_seconds));
        peer.options.alpn = ALPN::Custom(CustomALPN::new(H3_UPSTREAM_ALPN.to_vec()));
        peer.options.max_h2_streams = upstream.http3_max_concurrent_streams;
        PreparedH3Peer {
            peer,
            route: route.clone(),
        }
    });
    Ok(PreparedUpstream {
        peer,
        h3,
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

fn insert_security_headers(
    response: &mut ResponseHeader,
    handler: HandlerKind,
    tls: bool,
) -> Result<()> {
    response.headers.reserve(4);
    response.insert_typed_header(X_CONTENT_TYPE_OPTIONS, NOSNIFF);
    if tls {
        response.insert_typed_header(STRICT_TRANSPORT_SECURITY, HSTS_VALUE);
    }
    if matches!(
        handler,
        HandlerKind::Static | HandlerKind::Vaultwarden | HandlerKind::Couchdb
    ) {
        response.insert_typed_header(X_FRAME_OPTIONS, SAMEORIGIN);
        response.insert_typed_header(REFERRER_POLICY, REFERRER_POLICY_VALUE);
    }
    Ok(())
}

async fn send_empty(
    runtime: &RuntimeConfig,
    session: &mut Session,
    status: u16,
    handler: Option<HandlerKind>,
    tls: bool,
    http3: bool,
    headers: &[(&'static str, &str)],
) -> Result<bool> {
    let mut response = ResponseHeader::build(status, Some(headers.len() + 8)).unwrap();
    response.insert_typed_header(CONTENT_LENGTH, HeaderValue::from_static("0"));
    for (name, value) in headers {
        response.insert_header(*name, *value)?;
    }
    if let Some(handler) = handler
        && runtime.config.server.security_headers
    {
        insert_security_headers(&mut response, handler, tls)?;
    }
    if tls
        && !http3
        && let Some(alt_svc) = runtime.http3_alt_svc_header()
    {
        response.insert_typed_header(ALT_SVC, alt_svc.clone());
    }
    session
        .write_response_header(Box::new(response), true)
        .await?;
    Ok(true)
}

async fn upstream_tcp_reachable(address: &str) -> bool {
    tokio::time::timeout(
        Duration::from_millis(500),
        tokio::net::TcpStream::connect(address),
    )
    .await
    .is_ok_and(|result| result.is_ok())
}

async fn send_health_details(
    session: &mut Session,
    runtime: &RuntimeConfig,
    upstream_h3: &UpstreamH3Registry,
) -> Result<bool> {
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
            let connected = if upstream.protocol.uses_http3() {
                match upstream_h3.route(name) {
                    Some(route) if upstream.protocol == UpstreamProtocol::Http3 => {
                        route.is_available()
                    }
                    Some(route) => {
                        route.is_available() || upstream_tcp_reachable(&upstream.address).await
                    }
                    None => false,
                }
            } else {
                upstream_tcp_reachable(&upstream.address).await
            };
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
    use http::header::{
        CACHE_CONTROL, CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE,
    };

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
    fn http3_upstream_plan_exposes_direct_custom_peer() {
        use cloudflare_pingora::upstreams::peer::{ALPN, Peer};

        let config: Config = serde_saphyr::from_str(
            r#"
server:
  http_listen: ["127.0.0.1:38081"]
  https_listen: []
trusted_proxies: ["127.0.0.0/8"]
upstreams:
  origin:
    address: "127.0.0.1:28443"
    tls: true
    sni: origin.test
    protocol: http3
hosts:
  front:
    domains: ["front.test"]
    handler: navidrome-main
    upstream: origin
"#,
        )
        .unwrap();
        let runtime = Arc::new(RuntimeConfig::new(config).unwrap());
        let h3_runtime = crate::h3_runtime::start(1).unwrap();
        let upstream_h3 = crate::upstream_h3::start(runtime.clone(), Some(&h3_runtime)).unwrap();
        let gateway = Gateway::new(runtime, upstream_h3).unwrap();
        let host = gateway.host("front.test").unwrap();
        let ctx = RequestContext {
            plan_index: host.plan("/headers").unwrap(),
            ..RequestContext::default()
        };
        let plan = &gateway.routing.plans[ctx.plan_index];
        let h3 = plan
            .h3
            .as_ref()
            .expect("http3 upstream must expose an H3 plan");
        assert!(h3.route.should_use_direct_h3(false));
        let peer = gateway
            .precomputed_upstream_peer(&ctx)
            .expect("route must expose a prepared upstream peer");
        assert!(matches!(peer.get_alpn(), Some(ALPN::Custom(_))));
        assert!(std::ptr::eq(peer, &h3.peer));
    }

    #[test]
    fn ignores_spoofed_forwarded_for_from_untrusted_peer() {
        let runtime = runtime();
        let peer = "198.51.100.20".parse().unwrap();
        assert_eq!(resolve_client_ip(&runtime, peer, Some("192.0.2.10")), peer);
    }

    #[test]
    fn prepared_host_lookup_is_case_insensitive_and_peers_cache_pool_hash() {
        let gateway =
            Gateway::new(Arc::new(runtime()), Arc::new(UpstreamH3Registry::default())).unwrap();
        assert_eq!(
            gateway.host("app.example.com").unwrap().domain.as_ref(),
            "app.example.com"
        );
        let host = gateway.host("APP.EXAMPLE.COM:443").unwrap();
        assert_eq!(host.domain.as_ref(), "app.example.com");
        assert_eq!(
            gateway.host("app.example.com.").unwrap().domain.as_ref(),
            "app.example.com"
        );
        let plan = &gateway.routing.plans[host.plan("/rest/stream").unwrap()];
        assert!(plan.peer.cached_reuse_hash.is_some());
    }

    #[test]
    fn global_concurrent_limit_is_shared_across_client_ips() {
        let shared = GatewayShared::from_runtime(&runtime()).unwrap();
        let first = shared.global_concurrent.acquire(2).unwrap();
        let second = shared.global_concurrent.acquire(2).unwrap();
        assert!(shared.global_concurrent.acquire(2).is_none());
        drop(first);
        assert!(shared.global_concurrent.acquire(2).is_some());
        drop(second);
    }

    #[test]
    fn public_and_handoff_gateways_share_admission() {
        let runtime = Arc::new(runtime());
        let shared = Arc::new(GatewayShared::from_runtime(&runtime).unwrap());
        let public = Gateway::with_shared(
            runtime.clone(),
            Arc::new(UpstreamH3Registry::default()),
            shared.clone(),
        )
        .unwrap();
        let handoff =
            Gateway::with_shared(runtime, Arc::new(UpstreamH3Registry::default()), shared).unwrap();
        assert!(Arc::ptr_eq(&public.shared, &handoff.shared));

        let ip = "192.0.2.80".parse().unwrap();
        let permit = public
            .shared
            .active_requests
            .acquire(LimitZone::NavidromeStream, ip, 1)
            .unwrap();
        assert!(
            handoff
                .shared
                .active_requests
                .acquire(LimitZone::NavidromeStream, ip, 1)
                .is_none()
        );
        drop(permit);
        assert!(
            handoff
                .shared
                .active_requests
                .acquire(LimitZone::NavidromeStream, ip, 1)
                .is_some()
        );
    }

    #[test]
    fn precomputed_peer_is_available_for_h1_routes() {
        let gateway =
            Gateway::new(Arc::new(runtime()), Arc::new(UpstreamH3Registry::default())).unwrap();
        let host = gateway.host("app.example.com").unwrap();
        let ctx = RequestContext {
            plan_index: host.plan("/rest/stream").unwrap(),
            ..RequestContext::default()
        };
        let peer = gateway
            .precomputed_upstream_peer(&ctx)
            .expect("H1 route must expose a prepared peer");
        assert!(std::ptr::eq(
            peer,
            &gateway.routing.plans[ctx.plan_index].peer
        ));
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
        use crate::routing::vaultwarden_auth_path;
        assert!(vaultwarden_auth_path("/api/accounts/login"));
        assert!(vaultwarden_auth_path("/identity/connect/token/extra"));
        assert!(!vaultwarden_auth_path("/api/accounts/login-evil"));
    }

    #[test]
    fn doh_route_matches_only_the_exact_endpoint() {
        let mut plans = [None; RouteClass::ALL.len()];
        plans[RouteClass::Doh.index()] = Some(1);
        plans[RouteClass::AdguardUi.index()] = Some(2);
        let host = PreparedHost {
            domain: Arc::from("dns.example.com"),
            name: "dns".into(),
            handler: HandlerKind::AdguardDns,
            redirect_http: true,
            plans,
        };

        assert_eq!(host.plan("/dns-query"), Some(1));
        assert_eq!(host.plan("/dns-queryfoo"), Some(2));
        assert_eq!(host.plan("/dns-query/"), Some(2));
    }

    #[test]
    fn applies_compression_only_to_intended_routes() {
        for route in [RouteClass::NavidromeApi, RouteClass::NavidromeCover] {
            assert!(forwards_accept_encoding(route), "route={route:?}");
        }
        for route in [
            RouteClass::NavidromeStream,
            RouteClass::NavidromeGrpc,
            RouteClass::VaultwardenAuth,
            RouteClass::VaultwardenHub,
            RouteClass::Vaultwarden,
            RouteClass::Couchdb,
            RouteClass::Doh,
            RouteClass::AdguardUi,
        ] {
            assert!(!forwards_accept_encoding(route), "route={route:?}");
        }

        for route in [
            RouteClass::Vaultwarden,
            RouteClass::Couchdb,
            RouteClass::AdguardUi,
        ] {
            assert!(uses_downstream_compression(route), "route={route:?}");
        }
        for route in [
            RouteClass::NavidromeStream,
            RouteClass::NavidromeCover,
            RouteClass::NavidromeApi,
            RouteClass::NavidromeGrpc,
            RouteClass::VaultwardenAuth,
            RouteClass::VaultwardenHub,
            RouteClass::Doh,
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
        response
            .insert_header(CONTENT_TYPE, "application/grpc+json")
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
    fn te_trailers_survives_hop_stripping_and_other_te_tokens_do_not() {
        let mut downstream = RequestHeader::build(Method::POST, b"/pkg.Svc/Method", None).unwrap();
        downstream.insert_header(TE, "trailers").unwrap();
        let mut upstream = downstream.clone();
        strip_request_hop_headers(&downstream, &mut upstream).unwrap();
        assert_eq!(upstream.headers.get(TE).unwrap(), "trailers");

        let mut mixed = RequestHeader::build(Method::POST, b"/pkg.Svc/Method", None).unwrap();
        mixed.insert_header(TE, "gzip, trailers").unwrap();
        mixed.insert_header(CONNECTION, "te, keep-alive").unwrap();
        let mut upstream = mixed.clone();
        strip_request_hop_headers(&mixed, &mut upstream).unwrap();
        assert_eq!(upstream.headers.get(TE).unwrap(), "trailers");
        assert!(!upstream.headers.contains_key(CONNECTION));
        assert!(!upstream.headers.contains_key(KEEP_ALIVE));

        let mut gzip_only = RequestHeader::build(Method::GET, b"/", None).unwrap();
        gzip_only.insert_header(TE, "gzip").unwrap();
        let mut upstream = gzip_only.clone();
        strip_request_hop_headers(&gzip_only, &mut upstream).unwrap();
        assert!(!upstream.headers.contains_key(TE));
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
    fn connection_option_count_is_bounded() {
        let mut downstream = RequestHeader::build(Method::GET, b"/", None).unwrap();
        downstream
            .insert_header(CONNECTION, "x-1,x-2,x-3,x-4,x-5,x-6,x-7,x-8,x-9,x-10,x-11")
            .unwrap();
        let mut upstream = downstream.clone();
        assert!(strip_request_hop_headers(&downstream, &mut upstream).is_err());
    }

    #[test]
    fn content_length_is_reconciled_and_conflicts_are_rejected() {
        let mut headers = http::HeaderMap::new();
        headers.append(CONTENT_LENGTH, HeaderValue::from_static("5"));
        headers.append(CONTENT_LENGTH, HeaderValue::from_static("5, 5"));
        assert_eq!(validated_content_length(&headers), Ok(Some(5)));
        normalize_content_length_headers(&mut headers, 400).unwrap();
        assert_eq!(headers.get_all(CONTENT_LENGTH).iter().count(), 1);
        assert_eq!(headers[CONTENT_LENGTH], "5");

        headers.append(CONTENT_LENGTH, HeaderValue::from_static("6"));
        assert!(validated_content_length(&headers).is_err());
        for invalid in ["", "+5", "5x", "5,,5"] {
            let mut headers = http::HeaderMap::new();
            headers.insert(CONTENT_LENGTH, HeaderValue::from_str(invalid).unwrap());
            assert!(validated_content_length(&headers).is_err(), "{invalid:?}");
        }
    }

    #[test]
    fn transfer_encoding_cannot_conflict_or_cross_h2() {
        let mut request = RequestHeader::build(Method::POST, b"/", None).unwrap();
        request.insert_header(CONTENT_LENGTH, "5").unwrap();
        request.insert_header(TRANSFER_ENCODING, "chunked").unwrap();
        assert!(validated_request_content_length(&request).is_err());

        request.remove_header(&CONTENT_LENGTH);
        request.version = Version::HTTP_2;
        assert!(validated_request_content_length(&request).is_err());
    }

    #[test]
    fn duplicate_forwarded_for_fields_are_not_canonical() {
        let mut headers = http::HeaderMap::new();
        headers.append(X_FORWARDED_FOR, HeaderValue::from_static("192.0.2.1"));
        assert_eq!(canonical_forwarded_for(&headers), Ok(Some("192.0.2.1")));
        headers.append(X_FORWARDED_FOR, HeaderValue::from_static("198.51.100.2"));
        assert!(canonical_forwarded_for(&headers).is_err());
    }

    #[test]
    fn request_body_lifetime_is_bounded() {
        assert_eq!(request_body_lifetime(0), Duration::from_secs(60));
        assert_eq!(request_body_lifetime(usize::MAX), MAX_REQUEST_BODY_LIFETIME);
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
        response.insert_header("server", "upstream").unwrap();
        response.insert_header("x-powered-by", "framework").unwrap();
        response
            .insert_header(STRICT_TRANSPORT_SECURITY, "upstream-policy")
            .unwrap();
        strip_response_hop_headers(&mut response, false).unwrap();
        for name in [
            &CONNECTION,
            &KEEP_ALIVE,
            &PROXY_AUTHENTICATE,
            &HeaderName::from_static("x-private"),
            &HeaderName::from_static("server"),
            &HeaderName::from_static("x-powered-by"),
            &STRICT_TRANSPORT_SECURITY,
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
        let upstream = prepare_upstream("test", &upstream, &UpstreamH3Registry::default()).unwrap();
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
        let upstream = prepare_upstream("test", &upstream, &UpstreamH3Registry::default()).unwrap();
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
    fn navidrome_grpc_route_prefers_dedicated_h2c_upstream() {
        let config: Config = serde_saphyr::from_str(
            r#"
server:
  http_listen: ["127.0.0.1:38082"]
  https_listen: []
trusted_proxies: ["127.0.0.0/8"]
upstreams:
  app:
    address: "127.0.0.1:9000"
  navidrome_grpc:
    address: "127.0.0.1:50051"
    protocol: grpc
hosts:
  app:
    domains: ["app.example.com"]
    handler: navidrome-main
    upstream: app
"#,
        )
        .unwrap();
        let gateway = Gateway::new(
            Arc::new(RuntimeConfig::new(config).unwrap()),
            Arc::new(UpstreamH3Registry::default()),
        )
        .unwrap();
        let host = gateway.host("app.example.com").unwrap();
        let grpc_plan =
            &gateway.routing.plans[host.plans[RouteClass::NavidromeGrpc.index()].unwrap()];
        assert_eq!(grpc_plan.route, RouteClass::NavidromeGrpc);
        assert_eq!(grpc_plan.peer.options.alpn, ALPN::H2);
        let api_plan = &gateway.routing.plans[host.plan("/api/playlists").unwrap()];
        assert_eq!(api_plan.route, RouteClass::NavidromeApi);
        assert_eq!(api_plan.peer.options.alpn, ALPN::H1);
    }

    #[test]
    fn navidrome_grpc_route_falls_back_to_configured_upstream() {
        let config: Config = serde_saphyr::from_str(
            r#"
server:
  http_listen: ["127.0.0.1:38083"]
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
        .unwrap();
        let gateway = Gateway::new(
            Arc::new(RuntimeConfig::new(config).unwrap()),
            Arc::new(UpstreamH3Registry::default()),
        )
        .unwrap();
        let host = gateway.host("app.example.com").unwrap();
        let grpc_plan =
            &gateway.routing.plans[host.plans[RouteClass::NavidromeGrpc.index()].unwrap()];
        assert_eq!(grpc_plan.route, RouteClass::NavidromeGrpc);
        assert_eq!(grpc_plan.peer.options.alpn, ALPN::H1);
    }

    #[test]
    fn invalid_upstream_address_is_rejected_before_serving_requests() {
        let upstream: crate::config::UpstreamConfig =
            serde_saphyr::from_str("address: '127.0.0.1:not-a-port'").unwrap();
        let error =
            prepare_upstream("broken", &upstream, &UpstreamH3Registry::default()).unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("name=broken"));
        assert!(message.contains("127.0.0.1:not-a-port"));
    }

    #[test]
    fn tls_upstream_auto_prefers_h2_with_h1_fallback() {
        let upstream: crate::config::UpstreamConfig =
            serde_saphyr::from_str("address: 127.0.0.1:9443\ntls: true\nsni: upstream.test")
                .unwrap();
        let prepared = prepare_upstream("test", &upstream, &UpstreamH3Registry::default()).unwrap();
        assert_eq!(prepared.peer.options.alpn, ALPN::H2H1);
        assert_eq!(prepared.peer.options.max_h2_streams, 128);
        assert_eq!(
            prepared.peer.options.h2_stream_window_size,
            Some(2 * 1024 * 1024)
        );
        assert_eq!(
            prepared.peer.options.h2_connection_window_size,
            Some(8 * 1024 * 1024)
        );
        assert_eq!(
            prepared.peer.options.h2_ping_interval,
            Some(Duration::from_secs(30))
        );

        let hub = prepare_route_peer(&prepared, RouteClass::VaultwardenHub);
        assert_eq!(hub.options.alpn, ALPN::H1);
        assert_eq!(hub.options.max_h2_streams, 1);
    }

    #[test]
    fn plaintext_auto_stays_h1_and_explicit_http2_enables_h2c() {
        let automatic: crate::config::UpstreamConfig =
            serde_saphyr::from_str("address: 127.0.0.1:9000").unwrap();
        let automatic =
            prepare_upstream("auto", &automatic, &UpstreamH3Registry::default()).unwrap();
        assert_eq!(automatic.peer.options.alpn, ALPN::H1);

        let h2c: crate::config::UpstreamConfig = serde_saphyr::from_str(
            "address: 127.0.0.1:9000\nprotocol: http2\nhttp2_max_concurrent_streams: 64",
        )
        .unwrap();
        let h2c = prepare_upstream("h2c", &h2c, &UpstreamH3Registry::default()).unwrap();
        assert_eq!(h2c.peer.options.alpn, ALPN::H2);
        assert_eq!(h2c.peer.options.max_h2_streams, 64);

        let grpc: crate::config::UpstreamConfig =
            serde_saphyr::from_str("address: 127.0.0.1:50051\nprotocol: grpc").unwrap();
        let grpc = prepare_upstream("grpc", &grpc, &UpstreamH3Registry::default()).unwrap();
        assert_eq!(grpc.peer.options.alpn, ALPN::H2);
        let hub = prepare_route_peer(&grpc, RouteClass::VaultwardenHub);
        assert_eq!(hub.options.alpn, ALPN::H1);
        assert_eq!(hub.options.max_h2_streams, 1);
        assert!(prepare_route_h3(&grpc, RouteClass::VaultwardenHub).is_none());
        assert!(prepare_route_h3(&grpc, RouteClass::NavidromeGrpc).is_none());
    }

    #[test]
    fn navidrome_grpc_never_selects_upstream_http3() {
        let origin: std::net::SocketAddr = "127.0.0.1:443".parse().unwrap();
        let mut h3_peer = HttpPeer::new(origin, false, "navidrome".to_string());
        h3_peer.options.alpn = ALPN::Custom(CustomALPN::new(H3_UPSTREAM_ALPN.to_vec()));
        let upstream = PreparedUpstream {
            peer: HttpPeer::new(origin, true, "music.example".to_string()),
            h3: Some(PreparedH3Peer {
                peer: h3_peer,
                route: crate::upstream_h3::H3Route::preferred_for_tests(origin),
            }),
            read_timeout_seconds: None,
            write_timeout_seconds: None,
        };
        assert!(prepare_route_h3(&upstream, RouteClass::NavidromeGrpc).is_none());
        assert!(prepare_route_h3(&upstream, RouteClass::VaultwardenHub).is_none());
        assert!(prepare_route_h3(&upstream, RouteClass::NavidromeApi).is_some());
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
        assert_eq!(forwarded_port_value(Some(18_443), false).unwrap(), "18443");
        assert_eq!(forwarded_port_value(Some(18_080), false).unwrap(), "18080");
        assert_eq!(forwarded_port_value(Some(18_080), true).unwrap(), "18080");
        assert_eq!(forwarded_port_value(Some(80), true).unwrap(), "80");
        assert_eq!(forwarded_port_value(Some(443), false).unwrap(), "443");
        assert_eq!(forwarded_port_value(None, true).unwrap(), "443");
        assert_eq!(forwarded_port_value(None, false).unwrap(), "80");
    }

    #[test]
    fn forwarded_client_ip_header_cache_preserves_ipv4_and_ipv6_values() {
        let ipv4 = "192.0.2.17".parse().unwrap();
        let ipv6 = "2001:db8::17".parse().unwrap();

        assert_eq!(forwarded_client_ip_value(ipv4).unwrap(), "192.0.2.17");
        assert_eq!(forwarded_client_ip_value(ipv4).unwrap(), "192.0.2.17");
        assert_eq!(forwarded_client_ip_value(ipv6).unwrap(), "2001:db8::17");
        assert_eq!(forwarded_client_ip_value(ipv4).unwrap(), "192.0.2.17");
    }

    #[test]
    fn h1_bodyless_fast_path_keeps_upgrade_route_on_duplex_path() {
        assert!(!RouteClass::VaultwardenHub.supports_h1_bodyless_fast_path());
        for route in [
            RouteClass::NavidromeStream,
            RouteClass::NavidromeCover,
            RouteClass::NavidromeApi,
            RouteClass::NavidromeGrpc,
            RouteClass::VaultwardenAuth,
            RouteClass::Vaultwarden,
            RouteClass::Couchdb,
            RouteClass::Doh,
            RouteClass::AdguardUi,
        ] {
            assert!(route.supports_h1_bodyless_fast_path(), "{route:?}");
        }
    }

    #[test]
    fn h1_bodyless_poll_downstream_only_for_streaming_routes() {
        for (route, expected) in [
            (RouteClass::NavidromeStream, true),
            (RouteClass::NavidromeCover, true),
            (RouteClass::NavidromeGrpc, false),
            (RouteClass::Vaultwarden, false),
            (RouteClass::NavidromeApi, false),
        ] {
            assert_eq!(
                matches!(
                    route,
                    RouteClass::NavidromeStream | RouteClass::NavidromeCover
                ),
                expected,
                "{route:?}"
            );
        }
    }

    #[test]
    fn identity_only_accept_encoding_short_circuits() {
        let header = HeaderValue::from_static("identity");
        let mut values = [header].into_iter();
        let first = values.next().expect("accept-encoding value");
        assert!(values.next().is_none());
        assert!(first.as_bytes().eq_ignore_ascii_case(b"identity"));
    }
}
