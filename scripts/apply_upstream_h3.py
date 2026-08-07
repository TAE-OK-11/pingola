from pathlib import Path


def replace(path, old, new, count=1):
    p = Path(path)
    text = p.read_text()
    if old not in text:
        raise SystemExit(f"missing replacement in {path}: {old[:120]!r}")
    p.write_text(text.replace(old, new, count))

# Cargo: Hyper server is used only on the loopback h2c<->H3 bridge.
replace(
    "Cargo.toml",
    'hyper = { version = "1.11.0", features = ["client", "http2"] }',
    'hyper = { version = "1.11.0", features = ["client", "server", "http2"] }',
)

# Config defaults and fields.
replace(
    "src/config.rs",
    "fn default_upstream_http2_streams() -> usize {\n    32\n}\n",
    "fn default_upstream_http2_streams() -> usize {\n    32\n}\n\nfn default_upstream_http3_streams() -> usize {\n    64\n}\n",
)
replace(
    "src/config.rs",
    '    #[serde(default = "default_http3_handshake_timeout")]\n    pub http3_handshake_timeout_seconds: u64,',
    '    #[serde(default = "default_http3_handshake_timeout")]\n    pub http3_handshake_timeout_seconds: u64,\n    #[serde(default)]\n    pub http3_enable_early_data: bool,',
)
replace(
    "src/config.rs",
    '    #[serde(default = "default_upstream_http2_streams")]\n    pub http2_max_concurrent_streams: usize,',
    '    #[serde(default = "default_upstream_http2_streams")]\n    pub http2_max_concurrent_streams: usize,\n    #[serde(default = "default_upstream_http3_streams")]\n    pub http3_max_concurrent_streams: usize,\n    #[serde(default)]\n    pub http3_early_data: bool,',
)
replace(
    "src/config.rs",
    "pub enum UpstreamProtocol {\n    #[default]\n    Auto,\n    Http1,\n    Http2,\n}\n",
    "pub enum UpstreamProtocol {\n    #[default]\n    Auto,\n    Http1,\n    Http2,\n    Http3,\n    Http3Preferred,\n}\n\nimpl UpstreamProtocol {\n    pub fn uses_http3(self) -> bool {\n        matches!(self, Self::Http3 | Self::Http3Preferred)\n    }\n}\n",
)
replace(
    "src/config.rs",
    '        if !(1..=1024).contains(&upstream.http2_max_concurrent_streams) {\n            bail!("upstream {name} http2_max_concurrent_streams must be between 1 and 1024");\n        }',
    '        if !(1..=1024).contains(&upstream.http2_max_concurrent_streams) {\n            bail!("upstream {name} http2_max_concurrent_streams must be between 1 and 1024");\n        }\n        if !(1..=1024).contains(&upstream.http3_max_concurrent_streams) {\n            bail!("upstream {name} http3_max_concurrent_streams must be between 1 and 1024");\n        }\n        if upstream.protocol.uses_http3() && !upstream.tls {\n            bail!("HTTP/3 upstream {name} requires tls: true");\n        }',
)

# Gateway: dynamic selection between the local H3 bridge and existing Pingora TCP peer.
replace(
    "src/gateway.rs",
    'use crate::static_files::StaticFiles;\n',
    'use crate::static_files::StaticFiles;\nuse crate::upstream_h3::{BridgeRoute, UpstreamH3Registry};\n',
)
replace(
    "src/gateway.rs",
    "#[derive(Clone, Debug)]\nstruct PreparedUpstream {\n    peer: HttpPeer,\n    read_timeout_seconds: Option<u64>,\n    write_timeout_seconds: Option<u64>,\n}\n\n#[derive(Clone, Debug)]\nstruct PreparedPlan {\n    domain: http::HeaderValue,\n    handler: HandlerKind,\n    peer: HttpPeer,",
    "#[derive(Clone, Debug)]\nstruct PreparedH3Peer {\n    peer: HttpPeer,\n    route: BridgeRoute,\n}\n\n#[derive(Clone, Debug)]\nstruct PreparedUpstream {\n    peer: HttpPeer,\n    h3: Option<PreparedH3Peer>,\n    read_timeout_seconds: Option<u64>,\n    write_timeout_seconds: Option<u64>,\n}\n\n#[derive(Clone, Debug)]\nstruct PreparedPlan {\n    domain: http::HeaderValue,\n    handler: HandlerKind,\n    peer: HttpPeer,\n    h3: Option<PreparedH3Peer>,",
)
replace(
    "src/gateway.rs",
    "impl Gateway {\n    pub fn new(runtime: Arc<RuntimeConfig>) -> anyhow::Result<Self> {",
    "impl Gateway {\n    pub fn new(\n        runtime: Arc<RuntimeConfig>,\n        upstream_h3: Arc<UpstreamH3Registry>,\n    ) -> anyhow::Result<Self> {",
)
replace(
    "src/gateway.rs",
    "            .map(|(name, upstream)| {\n                prepare_upstream(name, upstream).map(|prepared| (name.clone(), prepared))\n            })",
    "            .map(|(name, upstream)| {\n                prepare_upstream(name, upstream, &upstream_h3)\n                    .map(|prepared| (name.clone(), prepared))\n            })",
)
replace(
    "src/gateway.rs",
    "                        peer: prepare_route_peer(upstream, route),\n                        route,",
    "                        peer: prepare_route_peer(upstream, route),\n                        h3: prepare_route_h3(upstream, route),\n                        route,",
)
replace(
    "src/gateway.rs",
    "        let plan = self.request_plan(ctx)?;\n        Ok(Box::new(plan.peer.clone()))\n    }\n\n    fn precomputed_upstream_peer<'a>(&'a self, ctx: &Self::CTX) -> Option<&'a HttpPeer> {\n        self.plans.get(ctx.plan_index).map(|plan| &plan.peer)\n    }",
    "        let plan = self.request_plan(ctx)?;\n        if let Some(h3) = &plan.h3\n            && h3.route.should_use_h3()\n        {\n            return Ok(Box::new(h3.peer.clone()));\n        }\n        Ok(Box::new(plan.peer.clone()))\n    }\n\n    fn precomputed_upstream_peer<'a>(&'a self, ctx: &Self::CTX) -> Option<&'a HttpPeer> {\n        self.plans\n            .get(ctx.plan_index)\n            .and_then(|plan| plan.h3.is_none().then_some(&plan.peer))\n    }",
)
replace(
    "src/gateway.rs",
    "fn prepare_route_peer(upstream: &PreparedUpstream, route: RouteClass) -> HttpPeer {",
    "fn prepare_route_h3(upstream: &PreparedUpstream, route: RouteClass) -> Option<PreparedH3Peer> {\n    if route == RouteClass::VaultwardenHub {\n        return None;\n    }\n    let mut h3 = upstream.h3.clone()?;\n    h3.peer.group_key = 10_000 + route.upstream_pool_group();\n    let (read_timeout, write_timeout) = upstream_timeouts(route, upstream);\n    h3.peer.options.read_timeout = Some(read_timeout);\n    h3.peer.options.write_timeout = Some(write_timeout);\n    h3.peer.cache_reuse_hash();\n    Some(h3)\n}\n\nfn prepare_route_peer(upstream: &PreparedUpstream, route: RouteClass) -> HttpPeer {",
)
replace(
    "src/gateway.rs",
    "fn prepare_upstream(\n    name: &str,\n    upstream: &crate::config::UpstreamConfig,\n) -> anyhow::Result<PreparedUpstream> {",
    "fn prepare_upstream(\n    name: &str,\n    upstream: &crate::config::UpstreamConfig,\n    upstream_h3: &UpstreamH3Registry,\n) -> anyhow::Result<PreparedUpstream> {",
)
replace(
    "src/gateway.rs",
    "        UpstreamProtocol::Auto if upstream.tls => ALPN::H2H1,\n        UpstreamProtocol::Auto | UpstreamProtocol::Http1 => ALPN::H1,\n        UpstreamProtocol::Http2 => ALPN::H2,\n    };",
    "        UpstreamProtocol::Auto | UpstreamProtocol::Http3 | UpstreamProtocol::Http3Preferred\n            if upstream.tls => ALPN::H2H1,\n        UpstreamProtocol::Auto | UpstreamProtocol::Http1 => ALPN::H1,\n        UpstreamProtocol::Http2 => ALPN::H2,\n        UpstreamProtocol::Http3 | UpstreamProtocol::Http3Preferred => ALPN::H1,\n    };",
)
replace(
    "src/gateway.rs",
    "    Ok(PreparedUpstream {\n        peer,\n        read_timeout_seconds: upstream.read_timeout_seconds,",
    "    let h3 = upstream_h3.route(name).map(|route| {\n        let mut peer = HttpPeer::new(route.address(), false, String::new());\n        peer.options.connection_timeout = Some(Duration::from_secs(upstream.connect_timeout_seconds));\n        peer.options.total_connection_timeout = Some(Duration::from_secs(upstream.connect_timeout_seconds));\n        peer.options.idle_timeout = Some(Duration::from_secs(upstream.idle_timeout_seconds));\n        peer.options.alpn = ALPN::H2;\n        peer.options.max_h2_streams = upstream.http3_max_concurrent_streams;\n        PreparedH3Peer {\n            peer,\n            route: route.clone(),\n        }\n    });\n    Ok(PreparedUpstream {\n        peer,\n        h3,\n        read_timeout_seconds: upstream.read_timeout_seconds,",
)
# Test helpers constructing Gateway need an empty registry.
text = Path("src/gateway.rs").read_text()
text = text.replace(
    "Gateway::new(Arc::new(runtime())).unwrap()",
    "Gateway::new(Arc::new(runtime()), Arc::new(UpstreamH3Registry::default())).unwrap()",
)
Path("src/gateway.rs").write_text(text)

# Main: start one upstream H3 bridge registry and share it with public + downstream-H3 handoff gateways.
replace("src/main.rs", "mod tls_policy;\n", "mod tls_policy;\nmod upstream_h3;\n")
replace(
    "src/main.rs",
    '    let gateway = Gateway::new(runtime.clone()).context("service bootstrap failed")?;',
    '    let upstream_h3 = upstream_h3::start(runtime.clone())\n        .context("upstream HTTP/3 bridge startup failed")?;\n    let gateway = Gateway::new(runtime.clone(), upstream_h3.clone())\n        .context("service bootstrap failed")?;',
)
replace(
    "src/main.rs",
    '            Gateway::new(runtime.clone()).context("HTTP/3 h2c service bootstrap failed")?;',
    '            Gateway::new(runtime.clone(), upstream_h3.clone())\n                .context("HTTP/3 h2c service bootstrap failed")?;',
)

# Downstream H3 server: optional replay-safe early data support for trusted/private deployments.
replace(
    "src/http3.rs",
    '    quic.enable_early_data = false;',
    '    quic.enable_early_data = server.http3_enable_early_data;',
)
replace(
    "src/http3.rs",
    "                                alt_svc: alt_svc.clone(),\n                            },",
    "                                alt_svc: alt_svc.clone(),\n                                allow_early_data: server.http3_enable_early_data,\n                            },",
)
replace(
    "src/http3.rs",
    '        "HTTP/3 frontend started: udp={:?} internal=h2c://{} quiche={} hybrid_pq={} stateless_retry=true max_amplification={} early_data=false migration=false pmtud=true pacing=true socket_offload=auto[gso,gro,so_txtime,rxq_ovfl,pmtu_probe] max_udp_payload={} send_capacity_factor={} admission_rate={}/s burst={} max_connections_per_ip={} handshake_timeout={}s",',
    '        "HTTP/3 frontend started: udp={:?} internal=h2c://{} quiche={} hybrid_pq={} stateless_retry=true max_amplification={} early_data={} migration=false pmtud=true pacing=true socket_offload=auto[gso,gro,so_txtime,rxq_ovfl,pmtu_probe] max_udp_payload={} send_capacity_factor={} admission_rate={}/s burst={} max_connections_per_ip={} handshake_timeout={}s",',
)
replace(
    "src/http3.rs",
    "        HTTP3_MAX_AMPLIFICATION_FACTOR,\n        HTTP3_MAX_UDP_PAYLOAD_SIZE,",
    "        HTTP3_MAX_AMPLIFICATION_FACTOR,\n        server.http3_enable_early_data,\n        HTTP3_MAX_UDP_PAYLOAD_SIZE,",
)
replace(
    "src/http3.rs",
    "    alt_svc: Option<HeaderValue>,\n}",
    "    alt_svc: Option<HeaderValue>,\n    allow_early_data: bool,\n}",
)
replace(
    "src/http3.rs",
    '''                if *is_in_early_data {\n                    warn!("HTTP/3 early-data request rejected peer={peer}");\n                    let IncomingH3Headers { mut send, .. } = incoming_headers;\n                    if let Err(error) = send_error(\n                        &mut send,\n                        StatusCode::TOO_EARLY,\n                        "HTTP/3 early data is not accepted",\n                    )\n                    .await\n                    {\n                        warn!("failed to reject HTTP/3 early-data request peer={peer}: {error:#}");\n                    }\n                    continue;\n                }\n                tokio::spawn(proxy_request(incoming_headers, context.clone()));''',
    '''                if *is_in_early_data\n                    && (!context.allow_early_data || !early_data_request_is_replay_safe(incoming_headers))\n                {\n                    warn!("HTTP/3 unsafe early-data request rejected peer={peer}");\n                    let IncomingH3Headers { mut send, .. } = incoming_headers;\n                    if let Err(error) = send_error(\n                        &mut send,\n                        StatusCode::TOO_EARLY,\n                        "HTTP/3 early data is limited to bodyless GET/HEAD",\n                    )\n                    .await\n                    {\n                        warn!("failed to reject HTTP/3 early-data request peer={peer}: {error:#}");\n                    }\n                    continue;\n                }\n                if *is_in_early_data {\n                    info!("HTTP/3 early-data request accepted peer={peer}");\n                }\n                tokio::spawn(proxy_request(incoming_headers, context.clone()));''',
)
# Insert helper before proxy_request.
replace(
    "src/http3.rs",
    "async fn proxy_request(incoming: IncomingH3Headers, context: Http3ConnectionContext) {",
    '''fn early_data_request_is_replay_safe(incoming: &IncomingH3Headers) -> bool {\n    if !incoming.read_fin {\n        return false;\n    }\n    incoming.headers.iter().find(|header| header.name() == b":method").is_some_and(|header| {\n        header.value().eq_ignore_ascii_case(b"GET")\n            || header.value().eq_ignore_ascii_case(b"HEAD")\n    })\n}\n\nasync fn proxy_request(incoming: IncomingH3Headers, context: Http3ConnectionContext) {''',
)
# Destructure the new context field without warnings.
replace(
    "src/http3.rs",
    "        client,\n        alt_svc,\n    } = context;",
    "        client,\n        alt_svc,\n        allow_early_data: _,\n    } = context;",
)

# Checked-in production config keeps public downstream 0-RTT off. Individual upstreams opt in.
replace(
    "config/pingora.yaml",
    "  http3_max_concurrent_streams: 64\n",
    "  http3_max_concurrent_streams: 64\n  http3_enable_early_data: false\n",
)

# README brief protocol docs.
replace(
    "README.md",
    "- HTTP/1.1 및 HTTP/2, 기본 최대 32개 동시 H2 stream(설정으로 1~1024 override)\n",
    "- HTTP/1.1 및 HTTP/2, 기본 최대 32개 동시 H2 stream(설정으로 1~1024 override)\n- upstream HTTP/3/QUIC (`http3`/`http3-preferred`) + connection reuse + replay-safe 0-RTT session resumption\n",
)
