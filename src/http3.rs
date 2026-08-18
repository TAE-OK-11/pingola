use std::cell::RefCell;
use std::convert::Infallible;
use std::error::Error as StdError;
use std::fmt::Write as _;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use boring::ssl::{SslContextBuilder, SslFiletype};
use bytes::Bytes;
use futures::{SinkExt, StreamExt, stream};
use http::header::{CONNECTION, HOST, TE, TRAILER, TRANSFER_ENCODING, UPGRADE};
use http::{HeaderMap, HeaderName, HeaderValue, Method, Request, StatusCode, Uri, Version};
use http_body_util::combinators::UnsyncBoxBody;
use http_body_util::{BodyExt, Empty, StreamBody};
use hyper::body::Frame;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use log::{error, info, warn};
use tokio::net::UdpSocket;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_quiche::http3::driver::{
    H3Event, InboundFrame, IncomingH3Headers, OutboundFrame, OutboundFrameSender,
    ServerH3Controller, ServerH3Event,
};
use tokio_quiche::http3::settings::Http3Settings;
use tokio_quiche::metrics::DefaultMetrics;
use tokio_quiche::quic::ConnectionHook;
use tokio_quiche::quiche::h3::{self, NameValue};
use tokio_quiche::settings::{CertificateKind, Hooks, QuicSettings, TlsCertificatePaths};
use tokio_quiche::socket::QuicListener;
use tokio_quiche::{ConnectionParams, ServerH3Driver, listen_with_capabilities};

use crate::config::RuntimeConfig;
use crate::limits::{ActiveRequestLimiter, ActiveRequestPermit, LimitZone, RateLimiter};
use crate::tls_policy::{HYBRID_PQ_GROUPS, new_hybrid_pq_context};

const INTERNAL_MARKER: &str = "x-jbs-http3-internal";
const INTERNAL_PORT: &str = "x-jbs-http3-port";
const STARTUP_TIMEOUT: Duration = Duration::from_secs(15);
const HTTP3_MAX_UDP_PAYLOAD_SIZE: usize = 1452;
const HTTP3_CONTROL_STREAM_LIMIT: u64 = 8;
const HTTP3_SEND_CAPACITY_FACTOR: f64 = 2.0;
const HTTP3_MAX_AMPLIFICATION_FACTOR: usize = 3;
// 1 vCPU / 1 GiB host: enough for Navidrome streams without the crate's
// 24 MiB connection / 16 MiB stream receive windows.
const HTTP3_INITIAL_MAX_DATA: u64 = 8 * 1024 * 1024;
const HTTP3_STREAM_WINDOW: u64 = 2 * 1024 * 1024;
const HTTP3_MAX_CONNECTION_WINDOW: u64 = 8 * 1024 * 1024;
const HTTP3_MAX_STREAM_WINDOW: u64 = 4 * 1024 * 1024;
const HTTP3_CC_BBR2: &str = "bbr2";

thread_local! {
    static CLIENT_IP_HEADER_CACHE: RefCell<Option<(IpAddr, HeaderValue)>> = const {
        RefCell::new(None)
    };
}

type BoxError = Box<dyn StdError + Send + Sync>;
type ProxyBody = UnsyncBoxBody<Bytes, BoxError>;
type ProxyClient = Client<HttpConnector, ProxyBody>;

#[derive(Debug)]
struct HybridPqQuicTlsHook;

impl ConnectionHook for HybridPqQuicTlsHook {
    fn create_custom_ssl_context_builder(
        &self,
        settings: TlsCertificatePaths<'_>,
    ) -> Option<SslContextBuilder> {
        Some(
            build_hybrid_pq_quic_context(settings.cert, settings.private_key).unwrap_or_else(
                |error| panic!("validated HTTP/3 hybrid PQ TLS context became invalid: {error:#}"),
            ),
        )
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

    fn admit(&self, peer: SocketAddr) -> Result<ActiveRequestPermit, Http3AdmissionRejection> {
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

pub fn start(runtime: Arc<RuntimeConfig>) -> Result<()> {
    let server = &runtime.config.server;
    if server.http3_listen.is_empty() {
        return Ok(());
    }

    let worker_threads = server.threads.clamp(1, 8);
    let (ready_tx, ready_rx) = mpsc::sync_channel::<Result<(), String>>(1);
    thread::Builder::new()
        .name("jbs-http3".to_string())
        .spawn(move || {
            let tokio_runtime = bounded_tokio_runtime(worker_threads, "jbs-h3-worker");
            let result = match tokio_runtime {
                Ok(tokio_runtime) => tokio_runtime.block_on(run(runtime, ready_tx)),
                Err(error) => {
                    let _ = ready_tx.send(Err(format!("HTTP/3 runtime creation failed: {error}")));
                    return;
                }
            };
            if let Err(error) = result {
                error!("HTTP/3 frontend stopped: {error:#}");
            }
        })
        .context("failed to spawn HTTP/3 runtime thread")?;

    ready_rx
        .recv_timeout(STARTUP_TIMEOUT)
        .map_err(|error| anyhow!("HTTP/3 startup did not complete: {error}"))?
        .map_err(anyhow::Error::msg)
}

async fn run(
    runtime: Arc<RuntimeConfig>,
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

    let mut connector = HttpConnector::new();
    connector.enforce_http(true);
    connector.set_connect_timeout(Some(Duration::from_secs(2)));
    // A single cleartext HTTP/2 connection multiplexes all concurrent H3
    // streams for this internal authority. There is intentionally no H1
    // fallback, so a protocol regression fails fast instead of becoming a
    // silent performance regression.
    let client: ProxyClient = Client::builder(TokioExecutor::new())
        .http2_only(true)
        // Match the trusted H3→H2c handoff windows. Adaptive windows can
        // grow past the 1 GiB host budget under concurrent audio streams.
        .http2_adaptive_window(false)
        .http2_initial_stream_window_size(2 * 1024 * 1024)
        .http2_initial_connection_window_size(32 * 1024 * 1024)
        .http2_max_frame_size(64 * 1024)
        .pool_max_idle_per_host(1)
        .build(connector);
    let internal = server.http3_internal_listen;
    let public_port = runtime
        .http3_public_port()
        .ok_or_else(|| anyhow!("HTTP/3 public port was not configured"))?;
    let public_port_header = HeaderValue::from_str(&public_port.to_string())
        .context("HTTP/3 public port is not a valid header value")?;
    let internal_token = runtime
        .http3_internal_token()
        .cloned()
        .ok_or_else(|| anyhow!("HTTP/3 internal token was not initialized"))?;
    let alt_svc = runtime.http3_alt_svc_header().cloned();
    let internal_uri_prefix = format!("http://{internal}");
    let shared = Arc::new(Http3Shared {
        internal_uri_prefix,
        public_port: public_port_header,
        internal_token,
        client,
        alt_svc,
        allow_early_data,
    });
    let max_requests_per_connection = u64::from(server.downstream_keepalive_requests);
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
                            max_requests_per_connection: Some(max_requests_per_connection),
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
        "HTTP/3 frontend started: udp={:?} internal=h2c://{} quiche={} hybrid_pq={} cc={} stateless_retry={} max_amplification={} early_data={} migration=false pmtud=true pacing=true socket_offload=auto[gso,gro,so_txtime,rxq_ovfl,pmtu_probe] max_udp_payload={} send_capacity_factor={} stream_window={} admission_rate={}/s burst={} max_connections_per_ip={} handshake_timeout={}s",
        server.http3_listen,
        internal,
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
    internal_uri_prefix: String,
    public_port: HeaderValue,
    internal_token: HeaderValue,
    client: ProxyClient,
    alt_svc: Option<HeaderValue>,
    allow_early_data: bool,
}

#[derive(Clone)]
struct Http3ConnectionContext {
    peer: SocketAddr,
    shared: Arc<Http3Shared>,
}

async fn handle_connection(
    mut controller: ServerH3Controller,
    context: Http3ConnectionContext,
    _connection_permit: OwnedSemaphorePermit,
    _client_connection_permit: ActiveRequestPermit,
) {
    let peer = context.peer;
    while let Some(event) = controller.event_receiver_mut().recv().await {
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
                    info!("HTTP/3 early-data request accepted peer={peer}");
                }
                tokio::spawn(proxy_request(incoming_headers, context.clone()));
            }
            ServerH3Event::Core(H3Event::BodyBytesReceived { .. }) => {}
            ServerH3Event::Core(event) => {
                log::debug!("HTTP/3 connection event peer={peer}: {event:?}");
            }
        }
    }
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
    if let Err(error) = proxy_request_inner(headers, send, recv, read_fin, context).await {
        warn!("HTTP/3 stream proxy failed peer={peer}: {error:#}");
    }
}

async fn proxy_request_inner(
    headers: Vec<h3::Header>,
    mut send: OutboundFrameSender,
    recv: tokio_quiche::http3::driver::InboundFrameStream,
    read_fin: bool,
    context: Http3ConnectionContext,
) -> Result<()> {
    let decoded = match decode_request_headers(
        &headers,
        context.peer,
        &context.shared.internal_uri_prefix,
        &context.shared.public_port,
        &context.shared.internal_token,
    ) {
        Ok(decoded) => decoded,
        Err(error) => {
            send_error(&mut send, StatusCode::BAD_REQUEST, "invalid HTTP/3 request").await?;
            return Err(error);
        }
    };
    if decoded.method == Method::CONNECT {
        send_error(
            &mut send,
            StatusCode::NOT_IMPLEMENTED,
            "HTTP/3 CONNECT is not supported",
        )
        .await?;
        return Ok(());
    }

    let body = request_body(recv, read_fin);
    let mut request = Request::builder()
        .method(decoded.method)
        .uri(decoded.uri)
        .version(Version::HTTP_2)
        .body(body)
        .context("failed to build internal HTTP/3 proxy request")?;
    *request.headers_mut() = decoded.headers;

    let response = tokio::time::timeout(
        Duration::from_secs(3600),
        context.shared.client.request(request),
    )
    .await
    .map_err(|_| anyhow!("internal Pingora h2c request timed out"))?
    .context("internal Pingora h2c request failed")?;
    forward_response(response, &mut send, context.shared.alt_svc.clone()).await
}

struct DecodedRequest {
    method: Method,
    uri: Uri,
    headers: HeaderMap,
}

fn decode_request_headers(
    headers: &[h3::Header],
    peer: SocketAddr,
    internal_uri_prefix: &str,
    public_port: &HeaderValue,
    internal_token: &HeaderValue,
) -> Result<DecodedRequest> {
    let mut method = None;
    let mut scheme = None;
    let mut authority = None;
    let mut path = None;
    let mut regular_seen = false;
    let mut output = HeaderMap::with_capacity(headers.len() + 6);

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
        output.append(
            name,
            HeaderValue::from_bytes(value).context("invalid HTTP/3 field value")?,
        );
    }

    let method = method.ok_or_else(|| anyhow!("missing :method"))?;
    let scheme = scheme.ok_or_else(|| anyhow!("missing :scheme"))?;
    if !scheme.eq_ignore_ascii_case(b"https") {
        bail!("HTTP/3 :scheme must be https");
    }
    let authority = authority.ok_or_else(|| anyhow!("missing :authority"))?;
    let authority = HeaderValue::from_bytes(authority).context("invalid :authority")?;
    let path = path.ok_or_else(|| anyhow!("missing :path"))?;
    let path = std::str::from_utf8(path).context(":path is not UTF-8")?;
    if !path.starts_with('/') {
        bail!("HTTP/3 :path must be origin-form");
    }
    let mut uri = String::with_capacity(internal_uri_prefix.len() + path.len());
    uri.push_str(internal_uri_prefix);
    uri.push_str(path);
    let uri: Uri = uri.parse().context("failed to construct internal URI")?;

    output.insert(HOST, authority);
    output.insert("x-forwarded-for", forwarded_client_ip_value(peer.ip())?);
    output.insert("x-forwarded-proto", HeaderValue::from_static("https"));
    output.insert("x-forwarded-port", public_port.clone());
    output.insert(INTERNAL_MARKER, internal_token.clone());
    output.insert(INTERNAL_PORT, public_port.clone());

    Ok(DecodedRequest {
        method,
        uri,
        headers: output,
    })
}

fn forwarded_client_ip_value(ip: IpAddr) -> Result<HeaderValue> {
    if let Some(value) = CLIENT_IP_HEADER_CACHE.with(|cache| {
        cache
            .borrow()
            .as_ref()
            .filter(|(cached, _)| *cached == ip)
            .map(|(_, value)| value.clone())
    }) {
        return Ok(value);
    }

    let mut encoded = arrayvec::ArrayString::<46>::new();
    write!(&mut encoded, "{ip}")
        .map_err(|error| anyhow!("client IP could not be formatted: {error}"))?;
    let value = HeaderValue::from_str(&encoded).context("client IP is not a valid header value")?;
    CLIENT_IP_HEADER_CACHE.with(|cache| {
        *cache.borrow_mut() = Some((ip, value.clone()));
    });
    Ok(value)
}

fn bounded_tokio_runtime(
    worker_threads: usize,
    thread_name: &'static str,
) -> std::io::Result<tokio::runtime::Runtime> {
    // A 1 vCPU host already runs Pingora's worker. A second multi-thread
    // runtime just contends for the same core and pins extra stacks.
    if worker_threads <= 1 {
        tokio::runtime::Builder::new_current_thread()
            .thread_name(thread_name)
            .enable_all()
            .build()
    } else {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(worker_threads)
            .thread_name(thread_name)
            .enable_all()
            .build()
    }
}

fn forbidden_request_header(name: &HeaderName, value: &[u8]) -> bool {
    name == CONNECTION
        || name.as_str() == "keep-alive"
        || name.as_str() == "proxy-connection"
        || name == TRANSFER_ENCODING
        || name == UPGRADE
        || (name == TE && !value.eq_ignore_ascii_case(b"trailers"))
}

fn request_body(
    recv: tokio_quiche::http3::driver::InboundFrameStream,
    read_fin: bool,
) -> ProxyBody {
    if read_fin {
        return Empty::<Bytes>::new()
            .map_err(infallible_to_box_error)
            .boxed_unsync();
    }

    let stream = stream::unfold((recv, false), |(mut recv, finished)| async move {
        if finished {
            return None;
        }
        loop {
            match recv.recv().await {
                Some(InboundFrame::Body(data, fin)) => {
                    // InboundFrame owns the BytesMut allocation. Freeze it into
                    // Bytes and transfer ownership into Hyper without copying.
                    let frame = Frame::data(data.freeze());
                    return Some((Ok::<_, BoxError>(frame), (recv, fin)));
                }
                Some(InboundFrame::Datagram(_)) => continue,
                None => return None,
            }
        }
    });
    StreamBody::new(stream).boxed_unsync()
}

fn infallible_to_box_error(error: Infallible) -> BoxError {
    match error {}
}

async fn forward_response(
    response: hyper::Response<hyper::body::Incoming>,
    send: &mut OutboundFrameSender,
    alt_svc: Option<HeaderValue>,
) -> Result<()> {
    let (parts, mut body) = response.into_parts();
    let mut headers = Vec::with_capacity(parts.headers.len() + 2);
    headers.push(h3::Header::new(
        b":status",
        parts.status.as_str().as_bytes(),
    ));
    let mut has_alt_svc = false;
    for (name, value) in &parts.headers {
        if forbidden_response_header(name) {
            continue;
        }
        has_alt_svc |= name.as_str() == "alt-svc";
        headers.push(h3::Header::new(name.as_str().as_bytes(), value.as_bytes()));
    }
    if !has_alt_svc && let Some(value) = alt_svc.as_ref() {
        headers.push(h3::Header::new(b"alt-svc", value.as_bytes()));
    }
    send.send(OutboundFrame::Headers(headers, None))
        .await
        .context("failed to send HTTP/3 response headers")?;

    while let Some(frame) = body.frame().await {
        let frame = frame.context("failed to read internal Pingora response body")?;
        if let Ok(data) = frame.into_data()
            && !data.is_empty()
        {
            send.send(OutboundFrame::Body(data, false))
                .await
                .context("failed to send HTTP/3 response body")?;
        }
    }
    send.send(OutboundFrame::Body(Bytes::new(), true))
        .await
        .context("failed to finish HTTP/3 response")?;
    Ok(())
}

fn forbidden_response_header(name: &HeaderName) -> bool {
    name == CONNECTION
        || name.as_str() == "keep-alive"
        || name.as_str() == "proxy-connection"
        || name == TRANSFER_ENCODING
        || name == UPGRADE
        || name == TRAILER
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
        let peer: SocketAddr = "192.0.2.44:443".parse().unwrap();

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
        assert!(
            decode_request_headers(
                &headers,
                "127.0.0.1:12345".parse().unwrap(),
                "http://127.0.0.1:18080",
                &HeaderValue::from_static("443"),
                &HeaderValue::from_static("unit-test-token"),
            )
            .is_err()
        );
    }

    #[test]
    fn request_header_decoder_builds_internal_request() {
        let headers = vec![
            h3::Header::new(b":method", b"GET"),
            h3::Header::new(b":scheme", b"https"),
            h3::Header::new(b":authority", b"music.example"),
            h3::Header::new(b":path", b"/rest/ping?x=1"),
            h3::Header::new(b"accept", b"application/json"),
        ];
        let request = decode_request_headers(
            &headers,
            "192.0.2.10:12345".parse().unwrap(),
            "http://127.0.0.1:18080",
            &HeaderValue::from_static("8443"),
            &HeaderValue::from_static("unit-test-token"),
        )
        .unwrap();
        assert_eq!(request.method, Method::GET);
        assert_eq!(
            request.uri.path_and_query().unwrap().as_str(),
            "/rest/ping?x=1"
        );
        assert_eq!(request.headers[HOST], "music.example");
        assert_eq!(request.headers[INTERNAL_MARKER], "unit-test-token");
        assert_eq!(request.headers["x-forwarded-for"], "192.0.2.10");
        assert_eq!(request.headers["x-forwarded-port"], "8443");
        assert_eq!(request.headers[INTERNAL_PORT], "8443");
    }

    #[test]
    fn forwarded_client_ip_header_cache_reuses_the_last_peer() {
        let ipv4 = "192.0.2.17".parse().unwrap();
        let ipv6 = "2001:db8::17".parse().unwrap();
        assert_eq!(forwarded_client_ip_value(ipv4).unwrap(), "192.0.2.17");
        assert_eq!(forwarded_client_ip_value(ipv4).unwrap(), "192.0.2.17");
        assert_eq!(forwarded_client_ip_value(ipv6).unwrap(), "2001:db8::17");
        assert_eq!(forwarded_client_ip_value(ipv4).unwrap(), "192.0.2.17");
    }
}
