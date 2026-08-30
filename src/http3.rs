use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use boring::ssl::{SslContextBuilder, SslFiletype};
use bytes::Bytes;
use cloudflare_pingora::apps::HttpServerApp;
use cloudflare_pingora::http::RequestHeader;
use cloudflare_pingora::protocols::http::server::Session as ServerSession;
use cloudflare_pingora::proxy::{HttpProxy, http_proxy_custom};
use cloudflare_pingora::server::ShutdownWatch;
use cloudflare_pingora::server::configuration::ServerConf;
use futures::stream::FuturesUnordered;
use futures::{SinkExt, StreamExt};
use http::header::{CONNECTION, HOST, HeaderName, TE, TRANSFER_ENCODING, UPGRADE};
use http::uri::{PathAndQuery, Scheme};
use http::{HeaderValue, Method, StatusCode, Uri, Version};
use log::{error, info, warn};
use tokio::net::UdpSocket;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, watch};
use tokio_quiche::http3::driver::{
    H3Event, IncomingH3Headers, OutboundFrame, OutboundFrameSender, ServerH3Controller,
    ServerH3Event,
};
use tokio_quiche::http3::settings::Http3Settings;
use tokio_quiche::metrics::DefaultMetrics;
use tokio_quiche::quic::ConnectionHook;
use tokio_quiche::quiche::h3::{self, NameValue};
use tokio_quiche::settings::{CertificateKind, Hooks, QuicSettings, TlsCertificatePaths};
use tokio_quiche::socket::QuicListener;
use tokio_quiche::{ConnectionParams, ServerH3Driver, listen_with_capabilities};

use crate::config::RuntimeConfig;
use crate::gateway::Gateway;
use crate::h3_session::H3Session;
use crate::limits::{ActiveRequestLimiter, ActiveRequestPermit, LimitZone, RateLimiter};
use crate::tls_policy::{HYBRID_PQ_GROUPS, new_hybrid_pq_context};
use crate::upstream_h3_connector::H3UpstreamConnector;

const STARTUP_TIMEOUT: Duration = Duration::from_secs(15);
const HTTP3_MAX_UDP_PAYLOAD_SIZE: usize = 1452;
const HTTP3_CONTROL_STREAM_LIMIT: u64 = 8;
const HTTP3_SEND_CAPACITY_FACTOR: f64 = 2.0;
const HTTP3_MAX_AMPLIFICATION_FACTOR: usize = 3;
// 1 vCPU / 1 GiB host: enough bandwidth-delay product for Navidrome streams
// without allowing slow consumers to retain multi-megabyte buffers per stream.
// Idle RSS is reclaimed via jemalloc background thread and connection-close hints.
const HTTP3_INITIAL_MAX_DATA: u64 = 4 * 1024 * 1024;
const HTTP3_STREAM_WINDOW: u64 = 1024 * 1024;
const HTTP3_MAX_CONNECTION_WINDOW: u64 = 8 * 1024 * 1024;
const HTTP3_MAX_STREAM_WINDOW: u64 = 2 * 1024 * 1024;
const HTTP3_CC_BBR2: &str = "bbr2";

#[derive(Debug)]
struct HybridPqQuicTlsHook;

impl ConnectionHook for HybridPqQuicTlsHook {
    fn create_custom_ssl_context_builder(
        &self,
        settings: TlsCertificatePaths<'_>,
    ) -> Option<SslContextBuilder> {
        match build_hybrid_pq_quic_context(settings.cert, settings.private_key) {
            Ok(builder) => Some(builder),
            Err(error) => {
                error!("validated HTTP/3 hybrid PQ TLS context became invalid: {error:#}");
                None
            }
        }
    }
}

fn build_hybrid_pq_quic_context(certificate: &str, private_key: &str) -> Result<SslContextBuilder> {
    let mut builder = new_hybrid_pq_context()
        .context("failed to create Cloudflare BoringSSL hybrid PQ context")?;
    builder
        .set_certificate_chain_file(certificate)
        .with_context(|| format!("failed to load HTTP/3 certificate chain {certificate}"))?;
    builder
        .set_private_key_file(private_key, SslFiletype::PEM)
        .with_context(|| format!("failed to load HTTP/3 private key {private_key}"))?;
    builder
        .check_private_key()
        .context("HTTP/3 certificate and private key do not match")?;
    Ok(builder)
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum Http3AdmissionRejection {
    RateLimited,
    TooManyConnections,
}

struct Http3Admission {
    rate: RateLimiter,
    active: ActiveRequestLimiter,
    rate_per_second: f64,
    burst: u32,
    max_active: usize,
}

impl Http3Admission {
    fn new(rate_per_second: f64, burst: u32, max_active: usize) -> Self {
        Self {
            rate: RateLimiter::new(),
            active: ActiveRequestLimiter::new(),
            rate_per_second,
            burst,
            max_active,
        }
    }

    fn admit(
        &self,
        peer: std::net::SocketAddr,
    ) -> Result<ActiveRequestPermit, Http3AdmissionRejection> {
        if !self.rate.allow(
            LimitZone::Http3Connection,
            peer.ip(),
            self.rate_per_second,
            self.burst,
        ) {
            return Err(Http3AdmissionRejection::RateLimited);
        }
        self.active
            .acquire(LimitZone::Http3Connection, peer.ip(), self.max_active)
            .ok_or(Http3AdmissionRejection::TooManyConnections)
    }
}

pub fn start(
    runtime: Arc<RuntimeConfig>,
    h3_runtime: &tokio::runtime::Handle,
    gateway: Gateway,
    server_conf: Arc<ServerConf>,
    h3_connector: H3UpstreamConnector,
) -> Result<()> {
    let server = &runtime.config.server;
    if server.http3_listen.is_empty() {
        return Ok(());
    }

    let (ready_tx, ready_rx) = mpsc::sync_channel::<Result<(), String>>(1);
    h3_runtime.spawn(async move {
        let failure_tx = ready_tx.clone();
        if let Err(error) = run(runtime, gateway, server_conf, h3_connector, ready_tx).await {
            let _ = failure_tx.send(Err(format!("HTTP/3 frontend startup failed: {error:#}")));
            error!("HTTP/3 frontend stopped: {error:#}");
        }
    });

    ready_rx
        .recv_timeout(STARTUP_TIMEOUT)
        .map_err(|error| anyhow!("HTTP/3 startup did not complete: {error}"))?
        .map_err(anyhow::Error::msg)
}

async fn run(
    runtime: Arc<RuntimeConfig>,
    gateway: Gateway,
    server_conf: Arc<ServerConf>,
    h3_connector: H3UpstreamConnector,
    ready: mpsc::SyncSender<Result<(), String>>,
) -> Result<()> {
    let server = &runtime.config.server;
    let allow_early_data = server.http3_enable_early_data;
    let stateless_retry = server.http3_stateless_retry;
    let certificate = server
        .certificate
        .as_deref()
        .ok_or_else(|| anyhow!("HTTP/3 requires server.certificate"))?;
    let private_key = server
        .private_key
        .as_deref()
        .ok_or_else(|| anyhow!("HTTP/3 requires server.private_key"))?;
    let certificate = certificate
        .to_str()
        .ok_or_else(|| anyhow!("HTTP/3 certificate path is not valid UTF-8"))?;
    let private_key = private_key
        .to_str()
        .ok_or_else(|| anyhow!("HTTP/3 private key path is not valid UTF-8"))?;

    // Build once before binding to guarantee the PQ group and certificate are
    // accepted. The connection hook repeats the same construction per listener
    // and fails closed if that invariant unexpectedly changes.
    drop(
        build_hybrid_pq_quic_context(certificate, private_key)
            .context("HTTP/3 hybrid PQ TLS preflight failed")?,
    );

    let mut quic_listeners = Vec::with_capacity(server.http3_listen.len());
    for address in &server.http3_listen {
        let socket = UdpSocket::bind(address)
            .await
            .with_context(|| format!("failed to bind HTTP/3 UDP listener {address}"))?;
        let mut listener = QuicListener::try_from(socket)
            .with_context(|| format!("failed to prepare HTTP/3 QUIC listener {address}"))?;
        // tokio-quiche listeners default every socket capability to OFF. On Linux
        // this best-effort probe enables the supported subset of UDP GSO/GRO,
        // SO_TXTIME pacing, RX queue overflow accounting, and PMTU probe sockopts.
        // Unsupported kernel/NIC capabilities remain disabled instead of failing
        // startup, so the same image stays portable across hosts.
        listener.apply_max_capabilities();
        info!(
            "HTTP/3 UDP offload capabilities: address={} capabilities={:?}",
            address, listener.capabilities
        );
        quic_listeners.push(listener);
    }

    let mut quic = QuicSettings::default();
    quic.enable_dgram = false;
    quic.dgram_recv_max_queue_len = 0;
    quic.dgram_send_max_queue_len = 0;
    quic.max_idle_timeout = Some(Duration::from_secs(server.http3_max_idle_timeout_seconds));
    quic.handshake_timeout = Some(Duration::from_secs(server.http3_handshake_timeout_seconds));
    quic.listen_backlog = server.downstream_max_connections.min(16_384);
    quic.initial_max_streams_bidi = u64::from(server.http3_max_concurrent_streams);
    quic.initial_max_streams_uni = HTTP3_CONTROL_STREAM_LIMIT;
    quic.initial_max_data = HTTP3_INITIAL_MAX_DATA;
    quic.initial_max_stream_data_bidi_local = HTTP3_STREAM_WINDOW;
    quic.initial_max_stream_data_bidi_remote = HTTP3_STREAM_WINDOW;
    quic.initial_max_stream_data_uni = HTTP3_STREAM_WINDOW;
    quic.max_connection_window = HTTP3_MAX_CONNECTION_WINDOW;
    quic.max_stream_window = HTTP3_MAX_STREAM_WINDOW;
    quic.max_recv_udp_payload_size = HTTP3_MAX_UDP_PAYLOAD_SIZE;
    quic.max_send_udp_payload_size = HTTP3_MAX_UDP_PAYLOAD_SIZE;
    quic.discover_path_mtu = true;
    quic.pmtud_max_probes = 3;
    // BBRv2 keeps lossy mobile paths from collapsing to CUBIC's conservative
    // window. HyStart++ does not apply to BBR.
    quic.cc_algorithm = HTTP3_CC_BBR2.to_string();
    quic.enable_relaxed_loss_threshold = true;
    // Keep QUIC packet sends paced instead of bursty. With the listener socket
    // capabilities enabled above, tokio-quiche can use SO_TXTIME where Linux
    // supports it and falls back to userspace pacing otherwise.
    quic.enable_pacing = true;
    quic.enable_hystart = false;
    quic.send_capacity_factor = HTTP3_SEND_CAPACITY_FACTOR;
    quic.enable_early_data = allow_early_data;
    quic.disable_active_migration = true;
    // Keep connection-ID/path state minimal. NAT rebinding can still be handled
    // sequentially while an attacker cannot queue multiple PATH_CHALLENGE frames.
    quic.active_connection_id_limit = 2;
    quic.max_path_challenge_recv_queue_len = 1;
    quic.grease = true;
    // Stateless Retry proves source-address ownership before the server allocates
    // a full QUIC connection and starts expensive TLS work. Keep it enabled by
    // default for public listeners; trusted private origins may explicitly turn
    // it off to permit true accepted 0-RTT without a Retry round trip.
    quic.disable_client_ip_validation = !stateless_retry;
    quic.max_amplification_factor = HTTP3_MAX_AMPLIFICATION_FACTOR;

    let params = ConnectionParams::new_server(
        quic,
        TlsCertificatePaths {
            cert: certificate,
            private_key,
            kind: CertificateKind::X509,
        },
        Hooks {
            connection_hook: Some(Arc::new(HybridPqQuicTlsHook)),
        },
    );
    let listeners = listen_with_capabilities(quic_listeners, params, DefaultMetrics)
        .context("failed to create quiche HTTP/3 listeners with UDP offload capabilities")?;

    let public_listen: std::net::SocketAddr = server.http3_listen[0]
        .parse()
        .context("HTTP/3 public listen address is not a valid socket address")?;
    let alt_svc = runtime
        .http3_alt_svc_header()
        .map(|value| Arc::new(value.clone()));
    let (_shutdown_tx, shutdown_rx) = watch::channel(false);
    let proxy = http_proxy_custom(server_conf.clone(), gateway, h3_connector);
    let proxy = Arc::new(proxy);
    let shared = Arc::new(Http3Shared {
        proxy,
        shutdown: shutdown_rx,
        public_listen,
        alt_svc,
        allow_early_data,
    });
    let max_requests_per_connection = server.http3_max_requests_per_connection;
    let max_streams_per_connection = server.http3_max_concurrent_streams as usize;
    let post_accept_timeout = Duration::from_secs(server.downstream_request_header_timeout_seconds);
    let connection_limit = Arc::new(Semaphore::new(server.downstream_max_connections));
    let admission = Arc::new(Http3Admission::new(
        server.http3_connection_rate_per_second,
        server.http3_connection_burst,
        server.http3_max_connections_per_ip,
    ));

    for mut listener in listeners {
        let shared = shared.clone();
        let connection_limit = connection_limit.clone();
        let admission = admission.clone();
        tokio::spawn(async move {
            while let Some(connection) = listener.next().await {
                match connection {
                    Ok(connection) => {
                        let permit = match connection_limit.clone().try_acquire_owned() {
                            Ok(permit) => permit,
                            Err(_) => {
                                warn!(
                                    "HTTP/3 connection rejected: downstream connection limit reached"
                                );
                                continue;
                            }
                        };
                        let peer = connection.peer_addr();
                        let client_permit = match admission.admit(peer) {
                            Ok(permit) => permit,
                            Err(Http3AdmissionRejection::RateLimited) => {
                                warn!(
                                    "HTTP/3 connection rejected: per-IP admission rate exceeded peer={peer}"
                                );
                                continue;
                            }
                            Err(Http3AdmissionRejection::TooManyConnections) => {
                                warn!(
                                    "HTTP/3 connection rejected: per-IP active connection limit reached peer={peer}"
                                );
                                continue;
                            }
                        };
                        let settings = Http3Settings {
                            max_requests_per_connection: Some(u64::from(
                                max_requests_per_connection,
                            )),
                            max_header_list_size: Some(64 * 1024),
                            post_accept_timeout: Some(post_accept_timeout),
                            ..Http3Settings::default()
                        };
                        let (driver, controller) = ServerH3Driver::new(settings);
                        connection.start(driver);
                        tokio::spawn(handle_connection(
                            controller,
                            Http3ConnectionContext {
                                peer,
                                shared: shared.clone(),
                                stream_slots: Arc::new(Semaphore::new(max_streams_per_connection)),
                            },
                            permit,
                            client_permit,
                        ));
                    }
                    Err(error) => warn!("HTTP/3 accept failed: {error}"),
                }
            }
        });
    }

    info!(
        "HTTP/3 frontend started: udp={:?} internal=direct-gateway quiche={} hybrid_pq={} cc={} stateless_retry={} max_amplification={} early_data={} migration=false pmtud=true pacing=true socket_offload=auto[gso,gro,so_txtime,rxq_ovfl,pmtu_probe] max_udp_payload={} send_capacity_factor={} stream_window={} admission_rate={}/s burst={} max_connections_per_ip={} handshake_timeout={}s",
        server.http3_listen,
        tokio_quiche::quiche::PROTOCOL_VERSION,
        HYBRID_PQ_GROUPS,
        HTTP3_CC_BBR2,
        stateless_retry,
        HTTP3_MAX_AMPLIFICATION_FACTOR,
        allow_early_data,
        HTTP3_MAX_UDP_PAYLOAD_SIZE,
        HTTP3_SEND_CAPACITY_FACTOR,
        HTTP3_STREAM_WINDOW,
        server.http3_connection_rate_per_second,
        server.http3_connection_burst,
        server.http3_max_connections_per_ip,
        server.http3_handshake_timeout_seconds,
    );
    let _ = ready.send(Ok(()));
    std::future::pending::<()>().await;
    Ok(())
}

struct Http3Shared {
    proxy: Arc<HttpProxy<Gateway, H3UpstreamConnector>>,
    shutdown: ShutdownWatch,
    public_listen: std::net::SocketAddr,
    alt_svc: Option<Arc<HeaderValue>>,
    allow_early_data: bool,
}

#[derive(Clone)]
struct Http3ConnectionContext {
    peer: std::net::SocketAddr,
    shared: Arc<Http3Shared>,
    stream_slots: Arc<Semaphore>,
}

async fn handle_connection(
    mut controller: ServerH3Controller,
    context: Http3ConnectionContext,
    _connection_permit: OwnedSemaphorePermit,
    _client_connection_permit: ActiveRequestPermit,
) {
    let peer = context.peer;
    let mut inflight = FuturesUnordered::new();
    loop {
        tokio::select! {
            biased;
            Some(_) = inflight.next(), if !inflight.is_empty() => {}
            event = controller.event_receiver_mut().recv() => {
                let Some(event) = event else {
                    break;
                };
                match event {
                    ServerH3Event::Headers {
                        incoming_headers,
                        is_in_early_data,
                        ..
                    } => {
                        if *is_in_early_data
                            && (!context.shared.allow_early_data
                                || !early_data_request_is_replay_safe(&incoming_headers))
                        {
                            warn!("HTTP/3 unsafe early-data request rejected peer={peer}");
                            let IncomingH3Headers { mut send, .. } = incoming_headers;
                            if let Err(error) = send_error(
                                &mut send,
                                StatusCode::TOO_EARLY,
                                "HTTP/3 early data is limited to bodyless GET/HEAD",
                            )
                            .await
                            {
                                warn!("failed to reject HTTP/3 early-data request peer={peer}: {error:#}");
                            }
                            continue;
                        }
                        if *is_in_early_data {
                            log::debug!("HTTP/3 early-data request accepted peer={peer}");
                        }
                        let stream_slots = context.stream_slots.clone();
                        let Ok(stream_permit) = stream_slots.try_acquire_owned() else {
                            warn!("HTTP/3 stream rejected: concurrent stream limit reached peer={peer}");
                            let IncomingH3Headers { mut send, .. } = incoming_headers;
                            if let Err(error) = send_error(
                                &mut send,
                                StatusCode::SERVICE_UNAVAILABLE,
                                "HTTP/3 concurrent stream limit reached",
                            )
                            .await
                            {
                                warn!(
                                    "failed to reject HTTP/3 stream-limit request peer={peer}: {error:#}"
                                );
                            }
                            continue;
                        };
                        let task_context = context.clone();
                        inflight.push(async move {
                            let _stream_permit = stream_permit;
                            proxy_request(incoming_headers, task_context).await
                        });
                    }
                    ServerH3Event::Core(H3Event::BodyBytesReceived { .. }) => {}
                    ServerH3Event::Core(event) => {
                        log::debug!("HTTP/3 connection event peer={peer}: {event:?}");
                    }
                }
            }
        }
    }
    while inflight.next().await.is_some() {}
    drop(context);
    crate::allocator::hint_release_idle_pages();
}

fn early_data_request_is_replay_safe(incoming: &IncomingH3Headers) -> bool {
    if !incoming.read_fin {
        return false;
    }
    incoming
        .headers
        .iter()
        .find(|header| header.name() == b":method")
        .is_some_and(|header| {
            header.value().eq_ignore_ascii_case(b"GET")
                || header.value().eq_ignore_ascii_case(b"HEAD")
        })
}

async fn proxy_request(incoming: IncomingH3Headers, context: Http3ConnectionContext) {
    let peer = context.peer;
    let IncomingH3Headers {
        headers,
        send,
        recv,
        read_fin,
        ..
    } = incoming;
    let request = match decode_request_headers(&headers) {
        Ok(request) => request,
        Err(error) => {
            let mut send = send;
            if let Err(send_error) =
                send_error(&mut send, StatusCode::BAD_REQUEST, "invalid HTTP/3 request").await
            {
                warn!(
                    "HTTP/3 invalid request peer={peer}: {error:#}; failed to send 400: {send_error:#}"
                );
            } else {
                warn!("HTTP/3 invalid request peer={peer}: {error:#}");
            }
            return;
        }
    };
    if request.method == Method::CONNECT {
        let mut send = send;
        if let Err(error) = send_error(
            &mut send,
            StatusCode::NOT_IMPLEMENTED,
            "HTTP/3 CONNECT is not supported",
        )
        .await
        {
            warn!("failed to reject HTTP/3 CONNECT peer={peer}: {error:#}");
        }
        return;
    }

    let session = ServerSession::new_custom(Box::new(H3Session::new(
        request,
        send,
        recv,
        read_fin,
        peer,
        context.shared.public_listen,
        context.shared.alt_svc.clone(),
    )));
    context
        .shared
        .proxy
        .process_new_http(session, &context.shared.shutdown)
        .await;
}

fn decode_request_headers(headers: &[h3::Header]) -> Result<RequestHeader> {
    if let Some(result) = decode_request_headers_fast(headers) {
        return result;
    }
    decode_request_headers_slow(headers)
}

fn decode_request_headers_fast(headers: &[h3::Header]) -> Option<Result<RequestHeader>> {
    if headers.is_empty() || headers.len() > 32 {
        return None;
    }
    let mut method = None;
    let mut scheme = false;
    let mut authority = None;
    let mut path = None;
    let mut regular_capacity = 0usize;
    for header in headers {
        let name = header.name();
        if name.starts_with(b":") {
            match name {
                b":method" if method.is_none() => method = Some(header.value()),
                b":scheme" if !scheme => {
                    if !header.value().eq_ignore_ascii_case(b"https") {
                        return Some(Err(anyhow!("HTTP/3 :scheme must be https")));
                    }
                    scheme = true;
                }
                b":authority" if authority.is_none() => authority = Some(header.value()),
                b":path" if path.is_none() => path = Some(header.value()),
                _ => return None,
            }
            continue;
        }
        if name.iter().any(u8::is_ascii_uppercase) {
            return None;
        }
        if name == b"host" {
            continue;
        }
        if name == b"connection"
            || name == b"keep-alive"
            || name == b"proxy-connection"
            || name == b"transfer-encoding"
            || name == b"upgrade"
            || (name == b"te" && !header.value().eq_ignore_ascii_case(b"trailers"))
        {
            return Some(Err(anyhow!(
                "HTTP/3 request contains a connection-specific field"
            )));
        }
        regular_capacity += 1;
    }
    let method = Method::from_bytes(method?).ok()?;
    if !scheme {
        return None;
    }
    let authority = std::str::from_utf8(authority?).ok()?;
    let path = std::str::from_utf8(path?).ok()?;
    if !path.starts_with('/') {
        return Some(Err(anyhow!("HTTP/3 :path must be origin-form")));
    }
    let host = HeaderValue::from_str(authority).ok()?;
    let uri = Uri::builder()
        .scheme(Scheme::HTTPS)
        .authority(authority)
        .path_and_query(path)
        .build()
        .ok()?;
    let mut request =
        RequestHeader::build_no_case(method, path.as_bytes(), Some(regular_capacity + 1)).ok()?;
    request.set_version(Version::HTTP_2);
    request.set_uri(uri);
    request.insert_typed_header(HOST, host);
    for header in headers {
        let name = header.name();
        if name.starts_with(b":") || name == b"host" {
            continue;
        }
        let name = HeaderName::from_bytes(name).ok()?;
        let value = HeaderValue::from_bytes(header.value()).ok()?;
        request.insert_typed_header(name, value);
    }
    Some(Ok(request))
}

fn decode_request_headers_slow(headers: &[h3::Header]) -> Result<RequestHeader> {
    let mut method = None;
    let mut scheme = None;
    let mut authority = None;
    let mut path = None;
    let mut regular_seen = false;
    let mut regular = Vec::with_capacity(headers.len());

    for header in headers {
        let name = header.name();
        let value = header.value();
        if name.starts_with(b":") {
            if regular_seen {
                bail!("HTTP/3 pseudo-header appears after a regular header");
            }
            match name {
                b":method" if method.is_none() => {
                    method = Some(Method::from_bytes(value).context("invalid :method")?);
                }
                b":scheme" if scheme.is_none() => scheme = Some(value),
                b":authority" if authority.is_none() => authority = Some(value),
                b":path" if path.is_none() => path = Some(value),
                _ => bail!("duplicate or unsupported HTTP/3 pseudo-header"),
            }
            continue;
        }
        regular_seen = true;
        if name.iter().any(u8::is_ascii_uppercase) {
            bail!("HTTP/3 field name contains uppercase bytes");
        }
        let name = HeaderName::from_bytes(name).context("invalid HTTP/3 field name")?;
        if forbidden_request_header(&name, value) {
            bail!("HTTP/3 request contains a connection-specific field: {name}");
        }
        if name == HOST {
            continue;
        }
        regular.push((
            name,
            HeaderValue::from_bytes(value).context("invalid HTTP/3 field value")?,
        ));
    }

    let method = method.ok_or_else(|| anyhow!("missing :method"))?;
    let scheme = scheme.ok_or_else(|| anyhow!("missing :scheme"))?;
    if !scheme.eq_ignore_ascii_case(b"https") {
        bail!("HTTP/3 :scheme must be https");
    }
    let authority = authority.ok_or_else(|| anyhow!("missing :authority"))?;
    let authority = std::str::from_utf8(authority).context(":authority is not UTF-8")?;
    let host = HeaderValue::from_str(authority).context("invalid :authority")?;
    let path = path.ok_or_else(|| anyhow!("missing :path"))?;
    let path = std::str::from_utf8(path).context(":path is not UTF-8")?;
    if !path.starts_with('/') {
        bail!("HTTP/3 :path must be origin-form");
    }
    let path = PathAndQuery::try_from(path).context("invalid HTTP/3 :path")?;
    let uri = Uri::builder()
        .scheme(Scheme::HTTPS)
        .authority(authority)
        .path_and_query(path.as_str())
        .build()
        .context("failed to construct HTTP/3 request URI")?;

    let header_count = regular.len() + 1;
    let mut request =
        RequestHeader::build_no_case(method, path.as_str().as_bytes(), Some(header_count))
            .map_err(|error| anyhow!("failed to build HTTP/3 request header: {error}"))?;
    // Pingora's HTTP/1 client can serialize HTTP/2 as HTTP/1.1 but panics on
    // HTTP/3. Direct QUIC sessions are identified with `is_custom()`, not the
    // wire version.
    request.set_version(Version::HTTP_2);
    request.set_uri(uri);
    request.insert_typed_header(HOST, host);
    for (name, value) in regular {
        request.insert_typed_header(name, value);
    }
    Ok(request)
}

fn forbidden_request_header(name: &HeaderName, value: &[u8]) -> bool {
    name == CONNECTION
        || name.as_str() == "keep-alive"
        || name.as_str() == "proxy-connection"
        || name == TRANSFER_ENCODING
        || name == UPGRADE
        || (name == TE && !value.eq_ignore_ascii_case(b"trailers"))
}

async fn send_error(
    send: &mut OutboundFrameSender,
    status: StatusCode,
    message: &'static str,
) -> Result<()> {
    let length = message.len().to_string();
    send.send(OutboundFrame::Headers(
        vec![
            h3::Header::new(b":status", status.as_str().as_bytes()),
            h3::Header::new(b"content-type", b"text/plain; charset=utf-8"),
            h3::Header::new(b"content-length", length.as_bytes()),
        ],
        None,
    ))
    .await
    .context("failed to send HTTP/3 error headers")?;
    send.send(OutboundFrame::Body(
        Bytes::from_static(message.as_bytes()),
        true,
    ))
    .await
    .context("failed to send HTTP/3 error body")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hybrid_pq_context_accepts_cloudflare_group_policy() {
        let mut builder = new_hybrid_pq_context().unwrap();
        builder.set_curves_list(HYBRID_PQ_GROUPS).unwrap();
    }

    #[test]
    fn http3_admission_limits_rate_and_active_connections() {
        let peer: std::net::SocketAddr = "192.0.2.44:443".parse().unwrap();

        let active = Http3Admission::new(10_000.0, 8, 1);
        let permit = active.admit(peer).unwrap();
        assert!(matches!(
            active.admit(peer),
            Err(Http3AdmissionRejection::TooManyConnections)
        ));
        drop(permit);

        let rate = Http3Admission::new(0.1, 0, 8);
        let _permit = rate.admit(peer).unwrap();
        assert!(matches!(
            rate.admit(peer),
            Err(Http3AdmissionRejection::RateLimited)
        ));
    }

    #[test]
    fn request_header_decoder_rejects_connection_fields() {
        let headers = vec![
            h3::Header::new(b":method", b"GET"),
            h3::Header::new(b":scheme", b"https"),
            h3::Header::new(b":authority", b"music.example"),
            h3::Header::new(b":path", b"/rest/ping"),
            h3::Header::new(b"connection", b"close"),
        ];
        assert!(decode_request_headers(&headers).is_err());
    }

    #[test]
    fn request_header_decoder_builds_public_https_request() {
        let headers = vec![
            h3::Header::new(b":method", b"GET"),
            h3::Header::new(b":scheme", b"https"),
            h3::Header::new(b":authority", b"music.example"),
            h3::Header::new(b":path", b"/rest/ping?x=1"),
            h3::Header::new(b"accept", b"application/json"),
        ];
        let request = decode_request_headers(&headers).unwrap();
        assert_eq!(request.method, Method::GET);
        assert_eq!(request.version, Version::HTTP_2);
        assert_eq!(request.uri.scheme_str(), Some("https"));
        assert_eq!(request.uri.authority().unwrap().as_str(), "music.example");
        assert_eq!(
            request.uri.path_and_query().unwrap().as_str(),
            "/rest/ping?x=1"
        );
        assert_eq!(request.headers[HOST], "music.example");
        assert_eq!(request.headers["accept"], "application/json");
        assert!(request.headers.get("x-jbs-http3-internal").is_none());
        assert!(request.headers.get("x-forwarded-for").is_none());
    }
}
