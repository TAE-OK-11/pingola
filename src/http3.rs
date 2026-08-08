use std::convert::Infallible;
use std::error::Error as StdError;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use boring::ssl::{SslContextBuilder, SslFiletype};
use boring::{base64, sha};
use bytes::Bytes;
use futures::{SinkExt, StreamExt, stream};
use http::header::{CONNECTION, CONTENT_LENGTH, HOST, TE, TRAILER, TRANSFER_ENCODING, UPGRADE};
use http::{HeaderMap, HeaderName, HeaderValue, Method, Request, StatusCode, Uri, Version};
use http_body_util::combinators::UnsyncBoxBody;
use http_body_util::{BodyExt, Empty, StreamBody};
use hyper::body::Frame;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use log::{error, info, warn};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UdpSocket};
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
use crate::limits::{ActiveRequestLimiter, ActiveRequestPermit, RateLimiter};
use crate::tls_policy::{HYBRID_PQ_GROUPS, new_hybrid_pq_context};

const INTERNAL_MARKER: &str = "x-jbs-http3-internal";
const INTERNAL_PORT: &str = "x-jbs-http3-port";
const STARTUP_TIMEOUT: Duration = Duration::from_secs(15);
const HTTP3_MAX_UDP_PAYLOAD_SIZE: usize = 1452;
const HTTP3_CONTROL_STREAM_LIMIT: u64 = 8;
const HTTP3_SEND_CAPACITY_FACTOR: f64 = 2.0;
const HTTP3_MAX_AMPLIFICATION_FACTOR: usize = 3;
const HTTP3_ADMISSION_ZONE: &str = "http3-connection";
const WEBSOCKET_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const WEBSOCKET_INTERNAL_CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const WEBSOCKET_HEADER_LIMIT: usize = 64 * 1024;
const WEBSOCKET_IO_BUFFER_SIZE: usize = 16 * 1024;
const WEBSOCKET_GUID: &[u8] = b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

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
            HTTP3_ADMISSION_ZONE,
            peer.ip(),
            self.rate_per_second,
            self.burst,
        ) {
            return Err(Http3AdmissionRejection::RateLimited);
        }
        self.active
            .acquire(HTTP3_ADMISSION_ZONE, peer.ip(), self.max_active)
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
            let tokio_runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(worker_threads)
                .thread_name("jbs-h3-worker")
                .enable_all()
                .build();
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
    quic.max_recv_udp_payload_size = HTTP3_MAX_UDP_PAYLOAD_SIZE;
    quic.max_send_udp_payload_size = HTTP3_MAX_UDP_PAYLOAD_SIZE;
    quic.discover_path_mtu = true;
    quic.pmtud_max_probes = 3;
    quic.enable_pacing = true;
    quic.enable_hystart = true;
    quic.send_capacity_factor = HTTP3_SEND_CAPACITY_FACTOR;
    quic.enable_early_data = allow_early_data;
    quic.disable_active_migration = true;
    quic.active_connection_id_limit = 2;
    quic.max_path_challenge_recv_queue_len = 1;
    quic.grease = true;
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
    let client: ProxyClient = Client::builder(TokioExecutor::new())
        .http2_only(true)
        .http2_adaptive_window(true)
        .pool_max_idle_per_host(1)
        .build(connector);
    let internal = server.http3_internal_listen;
    let public_port = runtime
        .http3_public_port()
        .ok_or_else(|| anyhow!("HTTP/3 public port was not configured"))?;
    let internal_token = runtime
        .http3_internal_token()
        .cloned()
        .ok_or_else(|| anyhow!("HTTP/3 internal token was not initialized"))?;
    let alt_svc = runtime.http3_alt_svc_header();
    let connection_limit = Arc::new(Semaphore::new(server.downstream_max_connections));
    let admission = Arc::new(Http3Admission::new(
        server.http3_connection_rate_per_second,
        server.http3_connection_burst,
        server.http3_max_connections_per_ip,
    ));

    for mut listener in listeners {
        let client = client.clone();
        let alt_svc = alt_svc.clone();
        let internal_token = internal_token.clone();
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
                            max_header_list_size: Some(64 * 1024),
                            enable_extended_connect: true,
                            ..Http3Settings::default()
                        };
                        let (driver, controller) = ServerH3Driver::new(settings);
                        connection.start(driver);
                        tokio::spawn(handle_connection(
                            controller,
                            Http3ConnectionContext {
                                peer,
                                internal,
                                public_port,
                                internal_token: internal_token.clone(),
                                client: client.clone(),
                                alt_svc: alt_svc.clone(),
                                allow_early_data,
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
        "HTTP/3 frontend started: udp={:?} internal=h2c://{} quiche={} hybrid_pq={} stateless_retry={} max_amplification={} early_data={} extended_connect=websocket migration=false pmtud=true pacing=true socket_offload=auto[gso,gro,so_txtime,rxq_ovfl,pmtu_probe] max_udp_payload={} send_capacity_factor={} admission_rate={}/s burst={} max_connections_per_ip={} handshake_timeout={}s",
        server.http3_listen,
        internal,
        tokio_quiche::quiche::PROTOCOL_VERSION,
        HYBRID_PQ_GROUPS,
        stateless_retry,
        HTTP3_MAX_AMPLIFICATION_FACTOR,
        allow_early_data,
        HTTP3_MAX_UDP_PAYLOAD_SIZE,
        HTTP3_SEND_CAPACITY_FACTOR,
        server.http3_connection_rate_per_second,
        server.http3_connection_burst,
        server.http3_max_connections_per_ip,
        server.http3_handshake_timeout_seconds,
    );
    let _ = ready.send(Ok(()));
    std::future::pending::<()>().await;
    Ok(())
}

#[derive(Clone)]
struct Http3ConnectionContext {
    peer: SocketAddr,
    internal: SocketAddr,
    public_port: u16,
    internal_token: HeaderValue,
    client: ProxyClient,
    alt_svc: Option<HeaderValue>,
    allow_early_data: bool,
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
                    && (!context.allow_early_data
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
    let Http3ConnectionContext {
        peer,
        internal,
        public_port,
        internal_token,
        client,
        alt_svc,
        allow_early_data: _,
    } = context;
    let IncomingH3Headers {
        headers,
        send,
        recv,
        read_fin,
        ..
    } = incoming;
    if let Err(error) = proxy_request_inner(
        headers,
        send,
        recv,
        read_fin,
        peer,
        internal,
        public_port,
        internal_token,
        client,
        alt_svc,
    )
    .await
    {
        warn!("HTTP/3 stream proxy failed peer={peer}: {error:#}");
    }
}

#[allow(clippy::too_many_arguments)]
async fn proxy_request_inner(
    headers: Vec<h3::Header>,
    mut send: OutboundFrameSender,
    recv: tokio_quiche::http3::driver::InboundFrameStream,
    read_fin: bool,
    peer: SocketAddr,
    internal: SocketAddr,
    public_port: u16,
    internal_token: HeaderValue,
    client: ProxyClient,
    alt_svc: Option<HeaderValue>,
) -> Result<()> {
    let decoded =
        match decode_request_headers(&headers, peer, internal, public_port, internal_token) {
            Ok(decoded) => decoded,
            Err(error) => {
                send_error(&mut send, StatusCode::BAD_REQUEST, "invalid HTTP/3 request").await?;
                return Err(error);
            }
        };
    if decoded.method == Method::CONNECT {
        if decoded
            .protocol
            .as_ref()
            .is_some_and(|protocol| protocol.as_bytes().eq_ignore_ascii_case(b"websocket"))
        {
            return proxy_websocket_extended_connect(
                decoded, send, recv, read_fin, internal, alt_svc,
            )
            .await;
        }
        send_error(
            &mut send,
            StatusCode::NOT_IMPLEMENTED,
            "HTTP/3 CONNECT protocol is not supported",
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

    let response = tokio::time::timeout(Duration::from_secs(3600), client.request(request))
        .await
        .map_err(|_| anyhow!("internal Pingora h2c request timed out"))?
        .context("internal Pingora h2c request failed")?;
    forward_response(response, &mut send, alt_svc).await
}

async fn proxy_websocket_extended_connect(
    decoded: DecodedRequest,
    mut send: OutboundFrameSender,
    recv: tokio_quiche::http3::driver::InboundFrameStream,
    read_fin: bool,
    internal: SocketAddr,
    alt_svc: Option<HeaderValue>,
) -> Result<()> {
    if read_fin {
        send_error(
            &mut send,
            StatusCode::BAD_REQUEST,
            "HTTP/3 WebSocket CONNECT stream is already closed",
        )
        .await?;
        return Ok(());
    }

    let key = generate_websocket_key()?;
    let request = build_internal_websocket_request(&decoded, &key)?;
    let mut stream = tokio::time::timeout(
        WEBSOCKET_INTERNAL_CONNECT_TIMEOUT,
        TcpStream::connect(internal),
    )
    .await
    .map_err(|_| anyhow!("internal Pingora WebSocket bridge connect timed out"))?
    .context("internal Pingora WebSocket bridge connect failed")?;
    stream
        .set_nodelay(true)
        .context("failed to enable TCP_NODELAY on internal WebSocket bridge")?;

    tokio::time::timeout(WEBSOCKET_HANDSHAKE_TIMEOUT, stream.write_all(&request))
        .await
        .map_err(|_| anyhow!("internal Pingora WebSocket handshake write timed out"))?
        .context("internal Pingora WebSocket handshake write failed")?;

    let (status, response_headers, buffered_body) = tokio::time::timeout(
        WEBSOCKET_HANDSHAKE_TIMEOUT,
        read_internal_websocket_response(&mut stream),
    )
    .await
    .map_err(|_| anyhow!("internal Pingora WebSocket handshake timed out"))??;

    if status != StatusCode::SWITCHING_PROTOCOLS {
        send_websocket_response_headers(&mut send, status, &response_headers, alt_svc.as_ref())
            .await?;
        send.send(OutboundFrame::Body(Bytes::new(), true))
            .await
            .context("failed to finish rejected HTTP/3 WebSocket CONNECT")?;
        return Ok(());
    }

    validate_internal_websocket_upgrade(&response_headers, &key)?;
    send_websocket_response_headers(
        &mut send,
        StatusCode::OK,
        &response_headers,
        alt_svc.as_ref(),
    )
    .await?;

    bridge_websocket_stream(recv, send, stream, buffered_body).await
}

fn generate_websocket_key() -> Result<String> {
    let mut nonce = [0_u8; 16];
    getrandom::fill(&mut nonce)
        .map_err(|error| anyhow!("failed to generate WebSocket handshake nonce: {error}"))?;
    Ok(base64::encode_block(&nonce))
}

fn websocket_accept_for_key(key: &str) -> String {
    let mut challenge = Vec::with_capacity(key.len() + WEBSOCKET_GUID.len());
    challenge.extend_from_slice(key.as_bytes());
    challenge.extend_from_slice(WEBSOCKET_GUID);
    base64::encode_block(&sha::sha1(&challenge))
}

fn build_internal_websocket_request(decoded: &DecodedRequest, key: &str) -> Result<Vec<u8>> {
    let path = decoded
        .uri
        .path_and_query()
        .map_or("/", |path| path.as_str());
    let host = decoded
        .headers
        .get(HOST)
        .ok_or_else(|| anyhow!("decoded HTTP/3 WebSocket request is missing Host"))?;
    let mut request = Vec::with_capacity(2048);
    request.extend_from_slice(b"GET ");
    request.extend_from_slice(path.as_bytes());
    request.extend_from_slice(b" HTTP/1.1\r\nHost: ");
    request.extend_from_slice(host.as_bytes());
    request.extend_from_slice(
        b"\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Key: ",
    );
    request.extend_from_slice(key.as_bytes());
    request.extend_from_slice(b"\r\n");

    for (name, value) in &decoded.headers {
        if forbidden_websocket_bridge_request_header(name) {
            continue;
        }
        request.extend_from_slice(name.as_str().as_bytes());
        request.extend_from_slice(b": ");
        request.extend_from_slice(value.as_bytes());
        request.extend_from_slice(b"\r\n");
    }
    if !decoded.headers.contains_key("sec-websocket-version") {
        request.extend_from_slice(b"Sec-WebSocket-Version: 13\r\n");
    }
    request.extend_from_slice(b"\r\n");
    if request.len() > WEBSOCKET_HEADER_LIMIT {
        bail!("internal WebSocket handshake exceeds header limit");
    }
    Ok(request)
}

fn forbidden_websocket_bridge_request_header(name: &HeaderName) -> bool {
    name == HOST
        || name == CONNECTION
        || name == CONTENT_LENGTH
        || name == TE
        || name == TRAILER
        || name == TRANSFER_ENCODING
        || name == UPGRADE
        || name.as_str() == "sec-websocket-key"
        || name.as_str() == "sec-websocket-accept"
}

async fn read_internal_websocket_response(
    stream: &mut TcpStream,
) -> Result<(StatusCode, HeaderMap, Bytes)> {
    let mut buffer = Vec::with_capacity(4096);
    let mut chunk = [0_u8; 4096];
    loop {
        if let Some(end) = find_http_header_end(&buffer) {
            let (status, headers) = parse_http1_response_head(&buffer[..end])?;
            return Ok((status, headers, Bytes::copy_from_slice(&buffer[end..])));
        }
        if buffer.len() >= WEBSOCKET_HEADER_LIMIT {
            bail!("internal WebSocket response headers exceed limit");
        }
        let read = stream
            .read(&mut chunk)
            .await
            .context("failed to read internal WebSocket handshake response")?;
        if read == 0 {
            bail!("internal WebSocket bridge closed before response headers completed");
        }
        buffer.extend_from_slice(&chunk[..read]);
        if buffer.len() > WEBSOCKET_HEADER_LIMIT {
            bail!("internal WebSocket response headers exceed limit");
        }
    }
}

fn find_http_header_end(buffer: &[u8]) -> Option<usize> {
    buffer
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|index| index + 4)
}

fn parse_http1_response_head(head: &[u8]) -> Result<(StatusCode, HeaderMap)> {
    let header_bytes = head
        .strip_suffix(b"\r\n\r\n")
        .ok_or_else(|| anyhow!("internal WebSocket response head is incomplete"))?;
    let mut lines = header_bytes.split(|byte| *byte == b'\n');
    let status_line = lines
        .next()
        .map(strip_line_cr)
        .ok_or_else(|| anyhow!("internal WebSocket response has no status line"))?;
    let status_line = std::str::from_utf8(status_line)
        .context("internal WebSocket response status line is not UTF-8")?;
    let mut status_parts = status_line.split_whitespace();
    let version = status_parts
        .next()
        .ok_or_else(|| anyhow!("internal WebSocket response is missing HTTP version"))?;
    if version != "HTTP/1.1" && version != "HTTP/1.0" {
        bail!("internal WebSocket response has unsupported HTTP version");
    }
    let status = status_parts
        .next()
        .ok_or_else(|| anyhow!("internal WebSocket response is missing status"))?
        .parse::<u16>()
        .context("invalid internal WebSocket response status")?;
    let status =
        StatusCode::from_u16(status).context("invalid internal WebSocket response status code")?;

    let mut headers = HeaderMap::new();
    for raw_line in lines {
        let line = strip_line_cr(raw_line);
        if line.is_empty() {
            continue;
        }
        if line.first().is_some_and(u8::is_ascii_whitespace) {
            bail!("internal WebSocket response contains obsolete folded header");
        }
        let colon = line
            .iter()
            .position(|byte| *byte == b':')
            .ok_or_else(|| anyhow!("internal WebSocket response contains malformed header"))?;
        let name = HeaderName::from_bytes(&line[..colon])
            .context("invalid internal WebSocket response header name")?;
        let value = HeaderValue::from_bytes(trim_ascii_header_value(&line[colon + 1..]))
            .context("invalid internal WebSocket response header value")?;
        headers.append(name, value);
    }
    Ok((status, headers))
}

fn strip_line_cr(line: &[u8]) -> &[u8] {
    line.strip_suffix(b"\r").unwrap_or(line)
}

fn trim_ascii_header_value(mut value: &[u8]) -> &[u8] {
    while value
        .first()
        .is_some_and(|byte| matches!(*byte, b' ' | b'\t'))
    {
        value = &value[1..];
    }
    while value
        .last()
        .is_some_and(|byte| matches!(*byte, b' ' | b'\t'))
    {
        value = &value[..value.len() - 1];
    }
    value
}

fn validate_internal_websocket_upgrade(headers: &HeaderMap, key: &str) -> Result<()> {
    if !header_contains_token(headers, "connection", b"upgrade") {
        bail!("internal WebSocket 101 is missing Connection: Upgrade");
    }
    if !header_contains_token(headers, "upgrade", b"websocket") {
        bail!("internal WebSocket 101 is missing Upgrade: websocket");
    }
    let mut accepts = headers.get_all("sec-websocket-accept").iter();
    let accept = accepts
        .next()
        .ok_or_else(|| anyhow!("internal WebSocket 101 is missing Sec-WebSocket-Accept"))?;
    if accepts.next().is_some() {
        bail!("internal WebSocket 101 has multiple Sec-WebSocket-Accept fields");
    }
    let expected = websocket_accept_for_key(key);
    if accept.as_bytes() != expected.as_bytes() {
        bail!("internal WebSocket Sec-WebSocket-Accept validation failed");
    }
    Ok(())
}

fn header_contains_token(headers: &HeaderMap, name: &'static str, token: &[u8]) -> bool {
    headers.get_all(name).iter().any(|value| {
        value
            .as_bytes()
            .split(|byte| *byte == b',')
            .map(trim_ascii_header_value)
            .any(|candidate| candidate.eq_ignore_ascii_case(token))
    })
}

async fn send_websocket_response_headers(
    send: &mut OutboundFrameSender,
    status: StatusCode,
    headers: &HeaderMap,
    alt_svc: Option<&HeaderValue>,
) -> Result<()> {
    let mut output = Vec::with_capacity(headers.len() + 2);
    output.push(h3::Header::new(b":status", status.as_str().as_bytes()));
    let mut has_alt_svc = false;
    for (name, value) in headers {
        if forbidden_websocket_bridge_response_header(name) {
            continue;
        }
        has_alt_svc |= name.as_str() == "alt-svc";
        output.push(h3::Header::new(name.as_str().as_bytes(), value.as_bytes()));
    }
    if !has_alt_svc && let Some(value) = alt_svc {
        output.push(h3::Header::new(b"alt-svc", value.as_bytes()));
    }
    send.send(OutboundFrame::Headers(output, None))
        .await
        .context("failed to send HTTP/3 WebSocket response headers")?;
    Ok(())
}

fn forbidden_websocket_bridge_response_header(name: &HeaderName) -> bool {
    forbidden_response_header(name)
        || name == CONTENT_LENGTH
        || name.as_str() == "sec-websocket-accept"
        || name.as_str() == "sec-websocket-key"
}

async fn bridge_websocket_stream(
    mut recv: tokio_quiche::http3::driver::InboundFrameStream,
    mut send: OutboundFrameSender,
    stream: TcpStream,
    buffered_body: Bytes,
) -> Result<()> {
    let (mut upstream_read, mut upstream_write) = stream.into_split();
    if !buffered_body.is_empty() {
        send.send(OutboundFrame::Body(buffered_body, false))
            .await
            .context("failed to forward buffered WebSocket bytes to HTTP/3 client")?;
    }

    let client_to_upstream = async {
        loop {
            match recv.recv().await {
                Some(InboundFrame::Body(data, fin)) => {
                    if !data.is_empty() {
                        upstream_write
                            .write_all(&data)
                            .await
                            .context("failed to forward HTTP/3 WebSocket bytes upstream")?;
                    }
                    if fin {
                        upstream_write
                            .shutdown()
                            .await
                            .context("failed to half-close internal WebSocket bridge")?;
                        return Ok::<(), anyhow::Error>(());
                    }
                }
                Some(InboundFrame::Datagram(_)) => continue,
                None => {
                    upstream_write
                        .shutdown()
                        .await
                        .context("failed to close internal WebSocket bridge")?;
                    return Ok(());
                }
            }
        }
    };

    let upstream_to_client = async {
        let mut buffer = vec![0_u8; WEBSOCKET_IO_BUFFER_SIZE];
        loop {
            let read = upstream_read
                .read(&mut buffer)
                .await
                .context("failed to read internal WebSocket bridge")?;
            if read == 0 {
                send.send(OutboundFrame::Body(Bytes::new(), true))
                    .await
                    .context("failed to finish HTTP/3 WebSocket stream")?;
                return Ok::<(), anyhow::Error>(());
            }
            send.send(OutboundFrame::Body(
                Bytes::copy_from_slice(&buffer[..read]),
                false,
            ))
            .await
            .context("failed to forward WebSocket bytes to HTTP/3 client")?;
        }
    };

    tokio::pin!(client_to_upstream);
    tokio::pin!(upstream_to_client);
    tokio::select! {
        result = &mut upstream_to_client => result,
        result = &mut client_to_upstream => {
            result?;
            upstream_to_client.await
        }
    }
}

struct DecodedRequest {
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    protocol: Option<HeaderValue>,
}

fn decode_request_headers(
    headers: &[h3::Header],
    peer: SocketAddr,
    internal: SocketAddr,
    public_port: u16,
    internal_token: HeaderValue,
) -> Result<DecodedRequest> {
    let mut method = None;
    let mut scheme = None;
    let mut authority = None;
    let mut path = None;
    let mut protocol = None;
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
                b":protocol" if protocol.is_none() => {
                    protocol = Some(HeaderValue::from_bytes(value).context("invalid :protocol")?);
                }
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
    if protocol.is_some() && method != Method::CONNECT {
        bail!("HTTP/3 :protocol requires CONNECT");
    }
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
    let uri: Uri = format!("http://{internal}{path}")
        .parse()
        .context("failed to construct internal URI")?;

    output.insert(HOST, authority);
    output.insert(
        "x-forwarded-for",
        HeaderValue::from_str(&peer.ip().to_string()).context("invalid client IP header")?,
    );
    output.insert("x-forwarded-proto", HeaderValue::from_static("https"));
    let public_port = HeaderValue::from_str(&public_port.to_string())
        .context("invalid HTTP/3 public port header")?;
    output.insert("x-forwarded-port", public_port.clone());
    output.insert(INTERNAL_MARKER, internal_token);
    output.insert(INTERNAL_PORT, public_port);

    Ok(DecodedRequest {
        method,
        uri,
        headers: output,
        protocol,
    })
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
                "127.0.0.1:18080".parse().unwrap(),
                443,
                HeaderValue::from_static("unit-test-token"),
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
            "127.0.0.1:18080".parse().unwrap(),
            8443,
            HeaderValue::from_static("unit-test-token"),
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
        assert!(request.protocol.is_none());
    }

    #[test]
    fn request_header_decoder_accepts_websocket_extended_connect() {
        let headers = vec![
            h3::Header::new(b":method", b"CONNECT"),
            h3::Header::new(b":protocol", b"websocket"),
            h3::Header::new(b":scheme", b"https"),
            h3::Header::new(b":authority", b"vault.example"),
            h3::Header::new(b":path", b"/notifications/hub"),
            h3::Header::new(b"sec-websocket-version", b"13"),
        ];
        let request = decode_request_headers(
            &headers,
            "192.0.2.20:12345".parse().unwrap(),
            "127.0.0.1:18080".parse().unwrap(),
            443,
            HeaderValue::from_static("unit-test-token"),
        )
        .unwrap();
        assert_eq!(request.method, Method::CONNECT);
        assert_eq!(request.protocol.as_ref().unwrap(), "websocket");
        assert_eq!(
            request.uri.path_and_query().unwrap().as_str(),
            "/notifications/hub"
        );
    }

    #[test]
    fn request_header_decoder_rejects_protocol_without_connect() {
        let headers = vec![
            h3::Header::new(b":method", b"GET"),
            h3::Header::new(b":protocol", b"websocket"),
            h3::Header::new(b":scheme", b"https"),
            h3::Header::new(b":authority", b"vault.example"),
            h3::Header::new(b":path", b"/notifications/hub"),
        ];
        assert!(
            decode_request_headers(
                &headers,
                "192.0.2.20:12345".parse().unwrap(),
                "127.0.0.1:18080".parse().unwrap(),
                443,
                HeaderValue::from_static("unit-test-token"),
            )
            .is_err()
        );
    }

    #[test]
    fn websocket_accept_matches_rfc6455_vector() {
        assert_eq!(
            websocket_accept_for_key("dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
    }

    #[test]
    fn internal_websocket_request_reconstructs_http1_upgrade() {
        let headers = vec![
            h3::Header::new(b":method", b"CONNECT"),
            h3::Header::new(b":protocol", b"websocket"),
            h3::Header::new(b":scheme", b"https"),
            h3::Header::new(b":authority", b"vault.example"),
            h3::Header::new(b":path", b"/notifications/hub?id=1"),
            h3::Header::new(b"origin", b"https://vault.example"),
            h3::Header::new(b"sec-websocket-protocol", b"json"),
        ];
        let decoded = decode_request_headers(
            &headers,
            "192.0.2.20:12345".parse().unwrap(),
            "127.0.0.1:18080".parse().unwrap(),
            443,
            HeaderValue::from_static("unit-test-token"),
        )
        .unwrap();
        let request =
            build_internal_websocket_request(&decoded, "dGhlIHNhbXBsZSBub25jZQ==").unwrap();
        let request = String::from_utf8(request).unwrap();
        assert!(request.starts_with("GET /notifications/hub?id=1 HTTP/1.1\r\n"));
        assert!(request.contains("Host: vault.example\r\n"));
        assert!(request.contains("Connection: Upgrade\r\n"));
        assert!(request.contains("Upgrade: websocket\r\n"));
        assert!(request.contains("Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n"));
        assert!(request.contains("Sec-WebSocket-Version: 13\r\n"));
        assert!(request.contains("x-jbs-http3-internal: unit-test-token\r\n"));
    }

    #[test]
    fn parses_and_validates_internal_websocket_upgrade() {
        let head = b"HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo=\r\nSec-WebSocket-Protocol: json\r\n\r\n";
        let (status, headers) = parse_http1_response_head(head).unwrap();
        assert_eq!(status, StatusCode::SWITCHING_PROTOCOLS);
        validate_internal_websocket_upgrade(&headers, "dGhlIHNhbXBsZSBub25jZQ==").unwrap();
        assert_eq!(headers["sec-websocket-protocol"], "json");
    }
}
