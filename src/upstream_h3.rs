use std::collections::{HashMap, VecDeque};
use std::error::Error as StdError;
use std::io;
use std::net::{
    IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener as StdTcpListener, ToSocketAddrs,
};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, mpsc as std_mpsc};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use bytes::Bytes;
use futures::stream;
use http::header::{CONNECTION, HOST, TE, TRANSFER_ENCODING, UPGRADE};
use http::{HeaderMap, HeaderName, HeaderValue, Method, Request, Response, StatusCode};
use http_body_util::combinators::UnsyncBoxBody;
use http_body_util::{BodyExt, StreamBody};
use hyper::body::{Body as _, Frame, Incoming};
use hyper::server::conn::http2;
use hyper::service::service_fn;
use hyper_util::rt::{TokioExecutor, TokioIo};
use log::{info, warn};
use tokio::net::{TcpListener, UdpSocket};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot};
use tokio_quiche::quiche;
use tokio_quiche::quiche::h3::{self, NameValue};

use crate::config::{RuntimeConfig, UpstreamConfig, UpstreamProtocol};
use crate::tls_policy::{HYBRID_PQ_GROUPS, new_hybrid_pq_context};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(15);
const MIN_RECONNECT_DELAY: Duration = Duration::from_millis(100);
const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(5);
const H3_RESPONSE_BACKPRESSURE_RETRY_DELAY: Duration = Duration::from_millis(1);
const MAX_UDP_PAYLOAD: usize = 1452;
const MAX_H3_HEADER_BYTES: u64 = 64 * 1024;
const MAX_REQUEST_COMMANDS: usize = 256;
const MAX_PENDING_REQUESTS: usize = 256;
const MAX_BODY_FRAMES: usize = 64;
const H3_BODY_RECV_BUFFER: usize = 16 * 1024;
const INITIAL_MAX_DATA: u64 = 64 * 1024 * 1024;
const INITIAL_STREAM_WINDOW: u64 = 16 * 1024 * 1024;
const H3_CONTROL_STREAMS: u64 = 8;
const SEND_CAPACITY_FACTOR: f64 = 2.0;
const CC_CUBIC: &str = "cubic";
const CC_BBR2: &str = "bbr2";

type BoxError = Box<dyn StdError + Send + Sync>;
type BridgeBody = UnsyncBoxBody<Bytes, BoxError>;

#[derive(Clone, Debug)]
pub struct BridgeRoute {
    address: SocketAddr,
    available: Arc<AtomicBool>,
    forced: bool,
}

impl BridgeRoute {
    pub fn address(&self) -> SocketAddr {
        self.address
    }

    pub fn should_use_h3(&self) -> bool {
        self.forced || self.available.load(Ordering::Acquire)
    }
}

#[derive(Default)]
pub struct UpstreamH3Registry {
    routes: HashMap<String, BridgeRoute>,
}

impl UpstreamH3Registry {
    pub fn route(&self, name: &str) -> Option<&BridgeRoute> {
        self.routes.get(name)
    }
}

#[derive(Clone)]
struct BridgeSettings {
    name: String,
    origin: SocketAddr,
    server_name: String,
    verify_peer: bool,
    connect_timeout: Duration,
    idle_timeout: Duration,
    max_streams: u64,
    enable_early_data: bool,
    cc_algorithm: &'static str,
}

struct BridgeSpec {
    listener: StdTcpListener,
    settings: BridgeSettings,
    available: Arc<AtomicBool>,
}

pub fn start(runtime: Arc<RuntimeConfig>) -> Result<Arc<UpstreamH3Registry>> {
    let mut routes = HashMap::new();
    let mut specs = Vec::new();

    for (name, upstream) in &runtime.config.upstreams {
        if !upstream.protocol.uses_http3() {
            continue;
        }
        if !upstream.tls {
            bail!("HTTP/3 upstream {name} requires tls: true");
        }

        let origin = resolve_origin(name, upstream)?;
        let server_name = upstream_server_name(name, upstream)?;
        let listener = StdTcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .with_context(|| format!("failed to allocate HTTP/3 bridge listener for {name}"))?;
        listener.set_nonblocking(true).with_context(|| {
            format!("failed to make HTTP/3 bridge listener nonblocking for {name}")
        })?;
        let address = listener.local_addr()?;
        let available = Arc::new(AtomicBool::new(false));
        let forced = upstream.protocol == UpstreamProtocol::Http3;

        routes.insert(
            name.clone(),
            BridgeRoute {
                address,
                available: available.clone(),
                forced,
            },
        );
        specs.push(BridgeSpec {
            listener,
            settings: BridgeSettings {
                name: name.clone(),
                origin,
                server_name,
                verify_peer: upstream.verify_certificate,
                connect_timeout: Duration::from_secs(upstream.connect_timeout_seconds),
                idle_timeout: Duration::from_secs(upstream.idle_timeout_seconds.max(1)),
                max_streams: upstream.http3_max_concurrent_streams as u64,
                enable_early_data: upstream.http3_early_data,
                cc_algorithm: if upstream.http3_bbr2 {
                    CC_BBR2
                } else {
                    CC_CUBIC
                },
            },
            available,
        });
    }

    let registry = Arc::new(UpstreamH3Registry { routes });
    if specs.is_empty() {
        return Ok(registry);
    }

    let (ready_tx, ready_rx) = std_mpsc::sync_channel::<Result<(), String>>(1);
    thread::Builder::new()
        .name("jbs-upstream-h3".to_string())
        .spawn(move || {
            let worker_threads = runtime.config.server.threads.clamp(1, 8);
            let tokio_runtime = if worker_threads <= 1 {
                tokio::runtime::Builder::new_current_thread()
                    .thread_name("jbs-upstream-h3-worker")
                    .enable_all()
                    .build()
            } else {
                tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(worker_threads)
                    .thread_name("jbs-upstream-h3-worker")
                    .enable_all()
                    .build()
            };
            let tokio_runtime = match tokio_runtime {
                Ok(runtime) => runtime,
                Err(error) => {
                    let _ = ready_tx.send(Err(format!(
                        "upstream HTTP/3 runtime creation failed: {error}"
                    )));
                    return;
                }
            };
            tokio_runtime.block_on(async move {
                for spec in specs {
                    let listener = match TcpListener::from_std(spec.listener) {
                        Ok(listener) => listener,
                        Err(error) => {
                            let _ = ready_tx.send(Err(format!(
                                "upstream HTTP/3 bridge listener conversion failed: {error}"
                            )));
                            return;
                        }
                    };
                    let pool = H3Pool::new(spec.settings.clone(), spec.available.clone());
                    tokio::spawn(serve_bridge(listener, pool));
                }
                let _ = ready_tx.send(Ok(()));
                std::future::pending::<()>().await;
            });
        })
        .context("failed to spawn upstream HTTP/3 runtime thread")?;

    ready_rx
        .recv_timeout(STARTUP_TIMEOUT)
        .map_err(|error| anyhow!("upstream HTTP/3 startup did not complete: {error}"))?
        .map_err(anyhow::Error::msg)?;

    for (name, route) in &registry.routes {
        info!(
            "upstream HTTP/3 bridge started: upstream={} h2c={} forced={} hybrid_pq={} early_data=replay-safe-only",
            name, route.address, route.forced, HYBRID_PQ_GROUPS,
        );
    }
    Ok(registry)
}

fn resolve_origin(name: &str, upstream: &UpstreamConfig) -> Result<SocketAddr> {
    upstream
        .address
        .to_socket_addrs()
        .with_context(|| {
            format!(
                "HTTP/3 upstream address resolution failed: name={name} address={}",
                upstream.address
            )
        })?
        .next()
        .ok_or_else(|| anyhow!("HTTP/3 upstream {name} resolved to no addresses"))
}

fn upstream_server_name(name: &str, upstream: &UpstreamConfig) -> Result<String> {
    if let Some(sni) = upstream.sni.as_ref().filter(|value| !value.is_empty()) {
        return Ok(sni.clone());
    }
    let authority = upstream
        .address
        .rsplit_once(':')
        .map_or(upstream.address.as_str(), |(host, _)| host)
        .trim_matches(['[', ']']);
    if authority.parse::<IpAddr>().is_ok() && upstream.verify_certificate {
        bail!("HTTP/3 upstream {name} with certificate verification requires sni");
    }
    Ok(authority.to_string())
}

#[derive(Clone)]
struct H3Pool {
    commands: mpsc::Sender<Command>,
    next_id: AtomicU64,
    request_slots: Arc<Semaphore>,
}

impl H3Pool {
    fn new(settings: BridgeSettings, available: Arc<AtomicBool>) -> Arc<Self> {
        let (commands, receiver) = mpsc::channel(MAX_REQUEST_COMMANDS);
        let pool = Arc::new(Self {
            commands,
            next_id: AtomicU64::new(1),
            request_slots: Arc::new(Semaphore::new(MAX_PENDING_REQUESTS)),
        });
        tokio::spawn(pool_manager(settings, receiver, available));
        pool
    }

    async fn open(
        &self,
        headers: Vec<h3::Header>,
        has_body: bool,
        allow_early_data: bool,
    ) -> Result<RequestHandle, BoxError> {
        let permit = self
            .request_slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| boxed_error("upstream HTTP/3 request capacity is exhausted"))?;
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (opened_tx, opened_rx) = oneshot::channel();
        let (response_tx, response_rx) = oneshot::channel();
        self.commands
            .send(Command::Open {
                id,
                headers,
                has_body,
                allow_early_data,
                cancel: self.commands.clone(),
                opened: opened_tx,
                response: response_tx,
                permit,
            })
            .await
            .map_err(|_| boxed_error("upstream HTTP/3 worker is unavailable"))?;
        Ok(RequestHandle {
            id,
            commands: self.commands.clone(),
            opened: Some(opened_rx),
            response: Some(response_rx),
            cancel_on_drop: true,
        })
    }
}

struct RequestHandle {
    id: u64,
    commands: mpsc::Sender<Command>,
    opened: Option<oneshot::Receiver<Result<(), String>>>,
    response: Option<oneshot::Receiver<Result<ResponseHead, String>>>,
    cancel_on_drop: bool,
}

impl RequestHandle {
    async fn wait_opened(&mut self) -> Result<(), BoxError> {
        let opened = self.opened.take().ok_or_else(|| {
            boxed_error("upstream HTTP/3 request open channel was already consumed")
        })?;
        opened
            .await
            .map_err(|_| boxed_error("upstream HTTP/3 request open channel closed"))?
            .map_err(boxed_error)
    }

    async fn response(mut self) -> Result<ResponseHead, BoxError> {
        let response = self
            .response
            .take()
            .ok_or_else(|| boxed_error("upstream HTTP/3 response channel was already consumed"))?;
        let response = response
            .await
            .map_err(|_| boxed_error("upstream HTTP/3 response channel closed"))?
            .map_err(boxed_error);
        self.cancel_on_drop = false;
        response
    }
}

impl Drop for RequestHandle {
    fn drop(&mut self) {
        if self.cancel_on_drop {
            enqueue_cancel(&self.commands, self.id);
        }
    }
}

struct ResponseHead {
    status: StatusCode,
    headers: HeaderMap,
    body: mpsc::Receiver<Result<Frame<Bytes>, String>>,
    finished: Arc<AtomicBool>,
    cancellation: ResponseCancellation,
}

struct ResponseCancellation {
    id: u64,
    commands: mpsc::Sender<Command>,
    finished: Arc<AtomicBool>,
}

impl Drop for ResponseCancellation {
    fn drop(&mut self) {
        if !self.finished.load(Ordering::Acquire) {
            enqueue_cancel(&self.commands, self.id);
        }
    }
}

fn enqueue_cancel(commands: &mpsc::Sender<Command>, id: u64) {
    let command = Command::Cancel { id };
    if let Err(mpsc::error::TrySendError::Full(command)) = commands.try_send(command)
        && let Ok(runtime) = tokio::runtime::Handle::try_current()
    {
        let commands = commands.clone();
        runtime.spawn(async move {
            let _ = commands.send(command).await;
        });
    }
}

enum Command {
    Open {
        id: u64,
        headers: Vec<h3::Header>,
        has_body: bool,
        allow_early_data: bool,
        cancel: mpsc::Sender<Command>,
        opened: oneshot::Sender<Result<(), String>>,
        response: oneshot::Sender<Result<ResponseHead, String>>,
        permit: OwnedSemaphorePermit,
    },
    Body {
        id: u64,
        data: Bytes,
        fin: bool,
        completed: oneshot::Sender<Result<(), String>>,
    },
    Trailers {
        id: u64,
        headers: Vec<h3::Header>,
        completed: oneshot::Sender<Result<(), String>>,
    },
    Cancel {
        id: u64,
    },
}

enum PendingWrite {
    Body {
        data: Bytes,
        offset: usize,
        fin: bool,
        completed: oneshot::Sender<Result<(), String>>,
    },
    Trailers {
        headers: Vec<h3::Header>,
        completed: oneshot::Sender<Result<(), String>>,
    },
}

impl PendingWrite {
    fn fail(self, message: &str) {
        let completed = match self {
            Self::Body { completed, .. } | Self::Trailers { completed, .. } => completed,
        };
        let _ = completed.send(Err(message.to_string()));
    }
}

struct PendingRequest {
    headers: Vec<h3::Header>,
    has_body: bool,
    allow_early_data: bool,
    cancel: mpsc::Sender<Command>,
    opened: Option<oneshot::Sender<Result<(), String>>>,
    response: Option<oneshot::Sender<Result<ResponseHead, String>>>,
    body_tx: mpsc::Sender<Result<Frame<Bytes>, String>>,
    body_rx: Option<mpsc::Receiver<Result<Frame<Bytes>, String>>>,
    response_finished: Arc<AtomicBool>,
    stream_id: Option<u64>,
    response_started: bool,
    pending_write: Option<PendingWrite>,
    _permit: OwnedSemaphorePermit,
}

impl PendingRequest {
    fn fail(mut self, message: &str) {
        if let Some(opened) = self.opened.take() {
            let _ = opened.send(Err(message.to_string()));
        }
        if let Some(response) = self.response.take() {
            let _ = response.send(Err(message.to_string()));
        } else {
            let _ = self.body_tx.try_send(Err(message.to_string()));
        }
        if let Some(write) = self.pending_write.take() {
            write.fail(message);
        }
    }
}

async fn pool_manager(
    settings: BridgeSettings,
    mut commands: mpsc::Receiver<Command>,
    available: Arc<AtomicBool>,
) {
    // The pool manager owns the cached ticket and invokes one QUIC connection
    // at a time, so this state is never concurrently accessed. Keep it local
    // instead of paying Arc/Mutex operations during reconnect/session updates.
    let mut session = None::<Vec<u8>>;
    let mut connected_once = false;
    let mut reconnect_delay = MIN_RECONNECT_DELAY;
    loop {
        let can_resume_early = connected_once && settings.enable_early_data && session.is_some();
        let initial_command = if can_resume_early {
            available.store(true, Ordering::Release);
            match commands.recv().await {
                Some(command) => Some(command),
                None => return,
            }
        } else {
            None
        };

        let result = run_connection(
            &settings,
            &mut session,
            &mut commands,
            available.clone(),
            initial_command,
        )
        .await;
        connected_once = true;
        let was_available = available.swap(false, Ordering::AcqRel);
        if commands.is_closed() {
            return;
        }
        if let Err(error) = result {
            warn!(
                "upstream HTTP/3 connection stopped upstream={}: {error:#}",
                settings.name
            );
        }
        let delay = if was_available {
            reconnect_delay = MIN_RECONNECT_DELAY;
            MIN_RECONNECT_DELAY
        } else {
            let delay = reconnect_delay;
            reconnect_delay = next_reconnect_delay(reconnect_delay);
            delay
        };
        tokio::time::sleep(reconnect_delay_with_jitter(delay)).await;
    }
}

fn next_reconnect_delay(current: Duration) -> Duration {
    current.saturating_mul(2).min(MAX_RECONNECT_DELAY)
}

fn reconnect_delay_with_jitter(delay: Duration) -> Duration {
    let mut entropy = [0_u8; 2];
    if getrandom::fill(&mut entropy).is_err() {
        return delay;
    }
    reconnect_delay_from_sample(delay, u16::from_ne_bytes(entropy))
}

fn reconnect_delay_from_sample(delay: Duration, sample: u16) -> Duration {
    let percentage = 80 + u32::from(sample) % 41;
    delay.mul_f64(f64::from(percentage) / 100.0)
}

async fn run_connection(
    settings: &BridgeSettings,
    session: &mut Option<Vec<u8>>,
    commands: &mut mpsc::Receiver<Command>,
    available: Arc<AtomicBool>,
    initial_command: Option<Command>,
) -> Result<()> {
    let bind = if settings.origin.is_ipv4() {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0)
    } else {
        SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 0)
    };
    let socket = UdpSocket::bind(bind)
        .await
        .with_context(|| format!("failed to bind UDP client for {}", settings.name))?;
    socket
        .connect(settings.origin)
        .await
        .with_context(|| format!("failed to connect UDP socket for {}", settings.name))?;
    let local = socket.local_addr()?;

    let mut tls = new_hybrid_pq_context()
        .context("failed to create upstream HTTP/3 Cloudflare BoringSSL context")?;
    if settings.verify_peer {
        tls.set_default_verify_paths()
            .context("failed to load default trust roots for HTTP/3 upstream")?;
    }
    let mut quic_config =
        quiche::Config::with_boring_ssl_ctx_builder(quiche::PROTOCOL_VERSION, tls)
            .context("failed to create upstream quiche configuration")?;
    quic_config
        .set_application_protos(h3::APPLICATION_PROTOCOL)
        .context("failed to configure HTTP/3 ALPN")?;
    quic_config
        .set_cc_algorithm_name(settings.cc_algorithm)
        .with_context(|| {
            format!(
                "failed to configure upstream HTTP/3 congestion control {}",
                settings.cc_algorithm
            )
        })?;
    quic_config.verify_peer(settings.verify_peer);
    quic_config.set_max_idle_timeout(settings.idle_timeout.as_millis() as u64);
    quic_config.set_max_recv_udp_payload_size(MAX_UDP_PAYLOAD);
    quic_config.set_max_send_udp_payload_size(MAX_UDP_PAYLOAD);
    quic_config.set_initial_max_data(INITIAL_MAX_DATA);
    quic_config.set_initial_max_stream_data_bidi_local(INITIAL_STREAM_WINDOW);
    quic_config.set_initial_max_stream_data_bidi_remote(INITIAL_STREAM_WINDOW);
    quic_config.set_initial_max_stream_data_uni(INITIAL_STREAM_WINDOW);
    quic_config.set_initial_max_streams_bidi(settings.max_streams);
    quic_config.set_initial_max_streams_uni(H3_CONTROL_STREAMS);
    quic_config.set_disable_active_migration(true);
    quic_config.set_active_connection_id_limit(2);
    quic_config.discover_pmtu(true);
    quic_config.set_pmtud_max_probes(3);
    quic_config.enable_hystart(true);
    quic_config.enable_pacing(true);
    quic_config.grease(true);
    quic_config.set_send_capacity_factor(SEND_CAPACITY_FACTOR);
    if settings.enable_early_data {
        quic_config.enable_early_data();
    }

    let mut scid_bytes = [0_u8; quiche::MAX_CONN_ID_LEN];
    getrandom::fill(&mut scid_bytes)
        .map_err(|error| anyhow!("failed to generate upstream QUIC connection ID: {error}"))?;
    let scid = quiche::ConnectionId::from_ref(&scid_bytes);
    let mut conn = quiche::connect(
        Some(&settings.server_name),
        &scid,
        local,
        settings.origin,
        &mut quic_config,
    )
    .with_context(|| format!("failed to create QUIC connection for {}", settings.name))?;

    if let Some(cached_session) = session.as_deref()
        && let Err(error) = conn.set_session(cached_session)
    {
        *session = None;
        bail!("cached upstream QUIC session was rejected and invalidated: {error}");
    }

    let mut h3_config = h3::Config::new().context("failed to create H3 config")?;
    h3_config.set_max_field_section_size(MAX_H3_HEADER_BYTES);
    let mut h3_conn = None;
    let mut requests = HashMap::<u64, PendingRequest>::new();
    let mut stream_to_request = HashMap::<u64, u64>::new();
    let mut waiting = VecDeque::<u64>::new();
    let mut pending_writes = VecDeque::<u64>::new();
    let mut recv_buf = vec![0_u8; MAX_UDP_PAYLOAD];
    let mut send_buf = vec![0_u8; MAX_UDP_PAYLOAD];
    let mut body_buf = vec![0_u8; H3_BODY_RECV_BUFFER];
    let mut pending_send = None;
    let handshake_deadline = Instant::now() + settings.connect_timeout;
    let mut established_logged = false;
    let mut session_logged = false;

    if let Some(command) = initial_command {
        handle_command(
            command,
            &mut conn,
            &mut requests,
            &mut stream_to_request,
            &mut waiting,
            &mut pending_writes,
        );
    }

    let result: Result<()> = async {
        loop {
            if conn.is_closed() {
                bail!("upstream QUIC connection closed");
            }
            if !conn.is_established() && Instant::now() >= handshake_deadline {
                conn.close(false, 0x1, b"handshake timeout").ok();
                bail!("upstream QUIC handshake timed out");
            }

            let app_ready = conn.is_established() || conn.is_in_early_data();
            if app_ready && h3_conn.is_none() {
                h3_conn = Some(
                    h3::Connection::with_transport(&mut conn, &h3_config)
                        .context("failed to create upstream HTTP/3 connection")?,
                );
            }
            if conn.is_established() {
                available.store(true, Ordering::Release);
                if !established_logged {
                    info!(
                        "upstream HTTP/3 established upstream={} peer={} resumed={} early_data_enabled={} cc={} hybrid_pq={}",
                        settings.name,
                        settings.origin,
                        conn.is_resumed(),
                        settings.enable_early_data,
                        settings.cc_algorithm,
                        HYBRID_PQ_GROUPS,
                    );
                    established_logged = true;
                }
                if let Some(new_session) = conn.session()
                    && session.as_deref() != Some(new_session)
                {
                    *session = Some(new_session.to_vec());
                    if !session_logged {
                        info!(
                            "upstream HTTP/3 session ticket cached upstream={}",
                            settings.name
                        );
                        session_logged = true;
                    }
                }
            }

            let response_backpressured = if let Some(h3_conn) = h3_conn.as_mut() {
                // Drop paths enqueue explicit Cancel commands. Avoid scanning and
                // allocating a temporary list of every active request on each
                // QUIC event-loop turn just to rediscover the same cancellations.
                dispatch_waiting(
                    h3_conn,
                    &mut conn,
                    &mut requests,
                    &mut stream_to_request,
                    &mut waiting,
                )?;
                drive_pending_writes(
                    h3_conn,
                    &mut conn,
                    &mut requests,
                    &mut stream_to_request,
                    &mut pending_writes,
                );
                process_h3_events(
                    h3_conn,
                    &mut conn,
                    &mut requests,
                    &mut stream_to_request,
                    &mut body_buf,
                )?
            } else {
                false
            };
            flush_ready_quic(&socket, &mut conn, &mut send_buf, &mut pending_send).await?;

            let timeout = conn.timeout().unwrap_or(Duration::from_secs(1));
            let pacing_delay = pending_send
                .as_ref()
                .map(|send: &ScheduledSend| send.at.saturating_duration_since(Instant::now()))
                .unwrap_or_default();
            tokio::select! {
                recv = socket.recv_from(&mut recv_buf), if !response_backpressured => {
                    let (len, from) = recv.context("upstream QUIC UDP receive failed")?;
                    let info = quiche::RecvInfo { from, to: local };
                    match conn.recv(&mut recv_buf[..len], info) {
                        Ok(_) | Err(quiche::Error::Done) => {}
                        Err(error) => return Err(anyhow!("upstream QUIC packet processing failed: {error:?}")),
                    }
                }
                command = commands.recv() => {
                    let Some(command) = command else {
                        conn.close(true, 0, b"shutdown").ok();
                        return Ok(());
                    };
                    handle_command(
                        command,
                        &mut conn,
                        &mut requests,
                        &mut stream_to_request,
                        &mut waiting,
                        &mut pending_writes,
                    );
                }
                _ = tokio::time::sleep(H3_RESPONSE_BACKPRESSURE_RETRY_DELAY), if response_backpressured => {}
                _ = tokio::time::sleep(timeout) => {
                    conn.on_timeout();
                }
                _ = tokio::time::sleep(pacing_delay), if pending_send.is_some() => {
                    let send = pending_send
                        .take()
                        .expect("pacing branch requires a scheduled QUIC packet");
                    send_quic_datagram(&socket, &send_buf[..send.len]).await?;
                }
            }
        }
    }
    .await;

    let message = result
        .as_ref()
        .err()
        .map_or("upstream QUIC worker stopped".to_string(), |error| {
            format!("upstream QUIC worker stopped: {error:#}")
        });
    fail_all(&mut requests, &message);
    result
}

fn handle_command(
    command: Command,
    conn: &mut quiche::Connection,
    requests: &mut HashMap<u64, PendingRequest>,
    stream_to_request: &mut HashMap<u64, u64>,
    waiting: &mut VecDeque<u64>,
    pending_writes: &mut VecDeque<u64>,
) {
    match command {
        Command::Open {
            id,
            headers,
            has_body,
            allow_early_data,
            cancel,
            opened,
            response,
            permit,
        } => {
            let (body_tx, body_rx) = mpsc::channel(MAX_BODY_FRAMES);
            let response_finished = Arc::new(AtomicBool::new(false));
            requests.insert(
                id,
                PendingRequest {
                    headers,
                    has_body,
                    allow_early_data,
                    cancel,
                    opened: Some(opened),
                    response: Some(response),
                    body_tx,
                    body_rx: Some(body_rx),
                    response_finished,
                    stream_id: None,
                    response_started: false,
                    pending_write: None,
                    _permit: permit,
                },
            );
            waiting.push_back(id);
        }
        Command::Body {
            id,
            data,
            fin,
            completed,
        } => {
            enqueue_write(
                id,
                PendingWrite::Body {
                    data,
                    offset: 0,
                    fin,
                    completed,
                },
                requests,
                pending_writes,
            );
        }
        Command::Trailers {
            id,
            headers,
            completed,
        } => {
            enqueue_write(
                id,
                PendingWrite::Trailers { headers, completed },
                requests,
                pending_writes,
            );
        }
        Command::Cancel { id } => {
            cancel_request(
                id,
                "downstream cancelled HTTP/3 request",
                conn,
                requests,
                stream_to_request,
            );
        }
    }
}

fn enqueue_write(
    id: u64,
    write: PendingWrite,
    requests: &mut HashMap<u64, PendingRequest>,
    pending_writes: &mut VecDeque<u64>,
) {
    let Some(request) = requests.get_mut(&id) else {
        write.fail("HTTP/3 request ended before its body was sent");
        return;
    };
    if request.stream_id.is_none() {
        write.fail("HTTP/3 request body arrived before the stream opened");
        return;
    }
    if request.pending_write.is_some() {
        write.fail("HTTP/3 request has more than one pending body write");
        return;
    }
    request.pending_write = Some(write);
    pending_writes.push_back(id);
}

fn drive_pending_writes(
    h3_conn: &mut h3::Connection,
    conn: &mut quiche::Connection,
    requests: &mut HashMap<u64, PendingRequest>,
    stream_to_request: &mut HashMap<u64, u64>,
    pending_writes: &mut VecDeque<u64>,
) {
    let count = pending_writes.len();
    for _ in 0..count {
        let Some(id) = pending_writes.pop_front() else {
            break;
        };
        let Some(request) = requests.get_mut(&id) else {
            continue;
        };
        let Some(stream_id) = request.stream_id else {
            continue;
        };
        let Some(mut write) = request.pending_write.take() else {
            continue;
        };

        let result = match &mut write {
            PendingWrite::Body {
                data, offset, fin, ..
            } => match h3_conn.send_body(conn, stream_id, &data[*offset..], *fin) {
                Ok(written) => {
                    *offset += written;
                    if *offset == data.len() {
                        Ok(true)
                    } else {
                        Ok(false)
                    }
                }
                Err(h3::Error::Done | h3::Error::StreamBlocked) => Ok(false),
                Err(error) => Err(format!("HTTP/3 request body send failed: {error:?}")),
            },
            PendingWrite::Trailers { headers, .. } => {
                match h3_conn.send_additional_headers(conn, stream_id, headers, true, true) {
                    Ok(()) => Ok(true),
                    Err(h3::Error::Done | h3::Error::StreamBlocked) => Ok(false),
                    Err(error) => Err(format!("HTTP/3 request trailer send failed: {error:?}")),
                }
            }
        };

        match result {
            Ok(true) => {
                let completed = match write {
                    PendingWrite::Body { completed, .. }
                    | PendingWrite::Trailers { completed, .. } => completed,
                };
                let _ = completed.send(Ok(()));
            }
            Ok(false) => {
                request.pending_write = Some(write);
                pending_writes.push_back(id);
            }
            Err(message) => {
                write.fail(&message);
                cancel_request(id, &message, conn, requests, stream_to_request);
            }
        }
    }
}

fn dispatch_waiting(
    h3_conn: &mut h3::Connection,
    conn: &mut quiche::Connection,
    requests: &mut HashMap<u64, PendingRequest>,
    stream_to_request: &mut HashMap<u64, u64>,
    waiting: &mut VecDeque<u64>,
) -> Result<()> {
    let count = waiting.len();
    for _ in 0..count {
        let Some(id) = waiting.pop_front() else { break };
        let Some(request) = requests.get_mut(&id) else {
            continue;
        };
        if request.stream_id.is_some() {
            continue;
        }
        let in_early_data = conn.is_in_early_data() && !conn.is_established();
        if in_early_data && !request.allow_early_data {
            waiting.push_back(id);
            continue;
        }
        if !conn.is_established() && !in_early_data {
            waiting.push_back(id);
            continue;
        }
        match h3_conn.send_request(conn, &request.headers, !request.has_body) {
            Ok(stream_id) => {
                request.stream_id = Some(stream_id);
                if in_early_data {
                    info!(
                        "upstream HTTP/3 early-data request sent stream={} request_id={}",
                        stream_id, id
                    );
                }
                stream_to_request.insert(stream_id, id);
                if let Some(opened) = request.opened.take() {
                    let _ = opened.send(Ok(()));
                }
            }
            Err(h3::Error::StreamBlocked)
            | Err(h3::Error::TransportError(quiche::Error::StreamLimit)) => {
                waiting.push_front(id);
                break;
            }
            Err(error) => {
                let request = requests.remove(&id).expect("request still exists");
                request.fail(&format!("HTTP/3 request open failed: {error:?}"));
            }
        }
    }
    Ok(())
}

fn process_h3_events(
    h3_conn: &mut h3::Connection,
    conn: &mut quiche::Connection,
    requests: &mut HashMap<u64, PendingRequest>,
    stream_to_request: &mut HashMap<u64, u64>,
    body_buf: &mut [u8],
) -> Result<bool> {
    loop {
        let (stream_id, event) = match h3_conn.poll(conn) {
            Ok(event) => event,
            Err(h3::Error::Done) => break,
            Err(error) => {
                return Err(anyhow!(
                    "upstream HTTP/3 event processing failed: {error:?}"
                ));
            }
        };
        let Some(request_id) = stream_to_request.get(&stream_id).copied() else {
            continue;
        };
        match event {
            h3::Event::Headers { list, .. } => {
                let response_started = requests
                    .get(&request_id)
                    .ok_or_else(|| anyhow!("HTTP/3 response has no request state"))?
                    .response_started;
                if !response_started {
                    let (status, headers) = match decode_response_headers(&list) {
                        Ok(decoded) => decoded,
                        Err(error) => {
                            let message = format!("invalid upstream HTTP/3 response: {error:#}");
                            warn!("{message} stream={stream_id}");
                            cancel_request(request_id, &message, conn, requests, stream_to_request);
                            continue;
                        }
                    };
                    if status.is_informational() {
                        continue;
                    }
                    let request = requests
                        .get_mut(&request_id)
                        .ok_or_else(|| anyhow!("HTTP/3 response has no request state"))?;
                    let body = request
                        .body_rx
                        .take()
                        .ok_or_else(|| anyhow!("HTTP/3 response body receiver missing"))?;
                    let response = request
                        .response
                        .take()
                        .ok_or_else(|| anyhow!("HTTP/3 response sender missing"))?;
                    request.response_started = true;
                    let finished = request.response_finished.clone();
                    let cancellation = ResponseCancellation {
                        id: request_id,
                        commands: request.cancel.clone(),
                        finished: finished.clone(),
                    };
                    if response
                        .send(Ok(ResponseHead {
                            status,
                            headers,
                            body,
                            finished,
                            cancellation,
                        }))
                        .is_err()
                    {
                        cancel_request(
                            request_id,
                            "downstream dropped HTTP/3 response headers",
                            conn,
                            requests,
                            stream_to_request,
                        );
                    }
                } else {
                    let trailers = match decode_trailers(&list) {
                        Ok(trailers) => trailers,
                        Err(error) => {
                            let message = format!("invalid upstream HTTP/3 trailers: {error:#}");
                            warn!("{message} stream={stream_id}");
                            cancel_request(request_id, &message, conn, requests, stream_to_request);
                            continue;
                        }
                    };
                    let send_result = requests
                        .get(&request_id)
                        .ok_or_else(|| anyhow!("HTTP/3 response has no request state"))?
                        .body_tx
                        .try_send(Ok(Frame::trailers(trailers)));
                    if let Err(error) = send_result {
                        let message = match error {
                            mpsc::error::TrySendError::Full(_) => {
                                "HTTP/3 response buffer limit exceeded while sending trailers"
                            }
                            mpsc::error::TrySendError::Closed(_) => {
                                "downstream dropped HTTP/3 response trailers"
                            }
                        };
                        cancel_request(request_id, message, conn, requests, stream_to_request);
                    }
                }
            }
            h3::Event::Data => loop {
                let body_tx = requests
                    .get(&request_id)
                    .ok_or_else(|| anyhow!("HTTP/3 data has no request state"))?
                    .body_tx
                    .clone();
                let permit = match body_tx.try_reserve() {
                    Ok(permit) => permit,
                    Err(mpsc::error::TrySendError::Full(_)) => return Ok(true),
                    Err(mpsc::error::TrySendError::Closed(_)) => {
                        cancel_request(
                            request_id,
                            "downstream dropped HTTP/3 response body",
                            conn,
                            requests,
                            stream_to_request,
                        );
                        break;
                    }
                };
                match h3_conn.recv_body(conn, stream_id, body_buf) {
                    Ok(read) if read > 0 => {
                        permit.send(Ok(Frame::data(Bytes::copy_from_slice(&body_buf[..read]))));
                    }
                    Ok(_) | Err(h3::Error::Done) => break,
                    Err(error) => {
                        return Err(anyhow!("HTTP/3 response body receive failed: {error:?}"));
                    }
                }
            },
            h3::Event::Finished => {
                stream_to_request.remove(&stream_id);
                if let Some(mut request) = requests.remove(&request_id) {
                    if request.response_started {
                        request.response_finished.store(true, Ordering::Release);
                    } else if let Some(response) = request.response.take() {
                        let _ =
                            response.send(Err("HTTP/3 response finished before headers".into()));
                    }
                    if let Some(write) = request.pending_write.take() {
                        write.fail("HTTP/3 response finished before request upload completed");
                    }
                }
            }
            h3::Event::Reset(code) => {
                let message = format!("HTTP/3 stream reset by upstream code={code}");
                cancel_request(request_id, &message, conn, requests, stream_to_request);
            }
            h3::Event::GoAway => {
                return Err(anyhow!("upstream HTTP/3 peer sent GOAWAY id={stream_id}"));
            }
            h3::Event::PriorityUpdate => {}
        }
    }
    Ok(false)
}

fn cancel_request(
    id: u64,
    message: &str,
    conn: &mut quiche::Connection,
    requests: &mut HashMap<u64, PendingRequest>,
    stream_to_request: &mut HashMap<u64, u64>,
) {
    let Some(request) = requests.remove(&id) else {
        return;
    };
    if let Some(stream_id) = request.stream_id {
        stream_to_request.remove(&stream_id);
        let code = h3::WireErrorCode::RequestCancelled as u64;
        let _ = conn.stream_shutdown(stream_id, quiche::Shutdown::Read, code);
        let _ = conn.stream_shutdown(stream_id, quiche::Shutdown::Write, code);
    }
    request.fail(message);
}

struct ScheduledSend {
    len: usize,
    at: Instant,
}

async fn flush_ready_quic(
    socket: &UdpSocket,
    conn: &mut quiche::Connection,
    out: &mut [u8],
    pending: &mut Option<ScheduledSend>,
) -> Result<()> {
    if pending.is_some() {
        return Ok(());
    }
    loop {
        let (write, send_info) = match conn.send(out) {
            Ok(value) => value,
            Err(quiche::Error::Done) => return Ok(()),
            Err(error) => return Err(anyhow!("upstream QUIC send generation failed: {error:?}")),
        };
        let now = Instant::now();
        if send_info.at > now {
            *pending = Some(ScheduledSend {
                len: write,
                at: send_info.at,
            });
            return Ok(());
        }
        send_quic_datagram(socket, &out[..write]).await?;
    }
}

async fn send_quic_datagram(socket: &UdpSocket, packet: &[u8]) -> Result<()> {
    let written = socket
        .send(packet)
        .await
        .context("upstream QUIC UDP send failed")?;
    if written != packet.len() {
        bail!(
            "upstream QUIC UDP send was truncated: expected={} written={written}",
            packet.len()
        );
    }
    Ok(())
}

fn fail_all(requests: &mut HashMap<u64, PendingRequest>, message: &str) {
    for (_, request) in requests.drain() {
        request.fail(message);
    }
}

async fn serve_bridge(listener: TcpListener, pool: Arc<H3Pool>) {
    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(value) => value,
            Err(error) => {
                warn!("upstream HTTP/3 h2c bridge accept failed: {error}");
                continue;
            }
        };
        if !peer.ip().is_loopback() {
            warn!("upstream HTTP/3 bridge rejected non-loopback peer={peer}");
            continue;
        }
        let pool = pool.clone();
        tokio::spawn(async move {
            let io = TokioIo::new(stream);
            let service = service_fn(move |request| proxy_bridge_request(request, pool.clone()));
            if let Err(error) = http2::Builder::new(TokioExecutor::new())
                .serve_connection(io, service)
                .await
            {
                log::debug!("upstream HTTP/3 h2c bridge connection ended: {error}");
            }
        });
    }
}

async fn proxy_bridge_request(
    request: Request<Incoming>,
    pool: Arc<H3Pool>,
) -> Result<Response<BridgeBody>, BoxError> {
    let (parts, mut body) = request.into_parts();
    let has_body = !body.is_end_stream();
    let allow_early_data = !has_body && matches!(parts.method, Method::GET | Method::HEAD);
    let headers = encode_request_headers(&parts)?;
    let mut handle = pool.open(headers, has_body, allow_early_data).await?;
    handle.wait_opened().await?;

    if has_body {
        let id = handle.id;
        let commands = handle.commands.clone();
        tokio::spawn(async move {
            let mut sent_fin = false;
            while let Some(frame) = body.frame().await {
                let frame = match frame {
                    Ok(frame) => frame,
                    Err(error) => {
                        warn!("h2c bridge request body read failed id={id}: {error}");
                        let _ = commands.send(Command::Cancel { id }).await;
                        return;
                    }
                };
                if let Some(data) = frame.data_ref()
                    && !data.is_empty()
                    && send_body_command(&commands, id, data.clone(), false)
                        .await
                        .is_err()
                {
                    let _ = commands.send(Command::Cancel { id }).await;
                    return;
                }
                if let Some(trailers) = frame.trailers_ref() {
                    let trailers = encode_regular_headers(trailers);
                    if send_trailers_command(&commands, id, trailers)
                        .await
                        .is_err()
                    {
                        let _ = commands.send(Command::Cancel { id }).await;
                        return;
                    }
                    sent_fin = true;
                }
            }
            if !sent_fin
                && send_body_command(&commands, id, Bytes::new(), true)
                    .await
                    .is_err()
            {
                let _ = commands.send(Command::Cancel { id }).await;
            }
        });
    }

    let ResponseHead {
        status,
        headers: response_headers,
        body,
        finished,
        cancellation,
    } = handle.response().await?;
    let mut builder = Response::builder().status(status);
    if let Some(headers) = builder.headers_mut() {
        *headers = response_headers;
    }
    let stream = response_body_stream(body, finished, cancellation);
    let body = StreamBody::new(stream).boxed_unsync();
    builder.body(body).map_err(|error| error.into())
}

async fn send_body_command(
    commands: &mpsc::Sender<Command>,
    id: u64,
    data: Bytes,
    fin: bool,
) -> Result<(), String> {
    let (completed, completion) = oneshot::channel();
    commands
        .send(Command::Body {
            id,
            data,
            fin,
            completed,
        })
        .await
        .map_err(|_| "upstream HTTP/3 worker is unavailable".to_string())?;
    completion
        .await
        .map_err(|_| "upstream HTTP/3 body completion channel closed".to_string())?
}

async fn send_trailers_command(
    commands: &mpsc::Sender<Command>,
    id: u64,
    headers: Vec<h3::Header>,
) -> Result<(), String> {
    let (completed, completion) = oneshot::channel();
    commands
        .send(Command::Trailers {
            id,
            headers,
            completed,
        })
        .await
        .map_err(|_| "upstream HTTP/3 worker is unavailable".to_string())?;
    completion
        .await
        .map_err(|_| "upstream HTTP/3 trailer completion channel closed".to_string())?
}

fn response_body_stream(
    body: mpsc::Receiver<Result<Frame<Bytes>, String>>,
    finished: Arc<AtomicBool>,
    cancellation: ResponseCancellation,
) -> impl futures::Stream<Item = Result<Frame<Bytes>, BoxError>> {
    stream::unfold(
        (body, finished, cancellation, false),
        |(mut body, finished, cancellation, reported_incomplete)| async move {
            match body.recv().await {
                Some(item) => Some((
                    item.map_err(boxed_error),
                    (body, finished, cancellation, reported_incomplete),
                )),
                None if finished.load(Ordering::Acquire) || reported_incomplete => None,
                None => Some((
                    Err(boxed_error(
                        "upstream HTTP/3 response ended before the stream finished",
                    )),
                    (body, finished, cancellation, true),
                )),
            }
        },
    )
}

fn encode_request_headers(parts: &http::request::Parts) -> Result<Vec<h3::Header>, BoxError> {
    let authority = parts
        .headers
        .get(HOST)
        .and_then(|value| value.to_str().ok())
        .or_else(|| parts.uri.authority().map(|value| value.as_str()))
        .ok_or_else(|| boxed_error("h2c bridge request is missing Host"))?;
    let path = parts
        .uri
        .path_and_query()
        .map_or("/", |value| value.as_str());
    let mut output = Vec::with_capacity(parts.headers.len() + 4);
    output.push(h3::Header::new(
        b":method",
        parts.method.as_str().as_bytes(),
    ));
    output.push(h3::Header::new(b":scheme", b"https"));
    output.push(h3::Header::new(b":authority", authority.as_bytes()));
    output.push(h3::Header::new(b":path", path.as_bytes()));
    output.extend(encode_regular_headers(&parts.headers));
    Ok(output)
}

fn encode_regular_headers(headers: &HeaderMap) -> Vec<h3::Header> {
    headers
        .iter()
        .filter(|(name, _)| {
            *name != HOST
                && *name != CONNECTION
                && *name != TRANSFER_ENCODING
                && *name != UPGRADE
                && name.as_str() != "keep-alive"
                && name.as_str() != "proxy-connection"
        })
        .filter(|(name, value)| *name != TE || value.as_bytes().eq_ignore_ascii_case(b"trailers"))
        .map(|(name, value)| h3::Header::new(name.as_str().as_bytes(), value.as_bytes()))
        .collect()
}

fn decode_response_headers(list: &[h3::Header]) -> Result<(StatusCode, HeaderMap)> {
    let mut status = None;
    let mut regular_seen = false;
    let mut headers = HeaderMap::with_capacity(list.len());
    for header in list {
        let name = header.name();
        if name.starts_with(b":") {
            if regular_seen || name != b":status" || status.is_some() {
                bail!("invalid HTTP/3 response pseudo-header ordering");
            }
            let value = std::str::from_utf8(header.value()).context("invalid :status encoding")?;
            let code = value.parse::<u16>().context("invalid :status value")?;
            status = Some(StatusCode::from_u16(code).context("invalid HTTP status")?);
            continue;
        }
        regular_seen = true;
        append_h3_header(&mut headers, header)?;
    }
    Ok((
        status.ok_or_else(|| anyhow!("HTTP/3 response missing :status"))?,
        headers,
    ))
}

fn decode_trailers(list: &[h3::Header]) -> Result<HeaderMap> {
    let mut headers = HeaderMap::with_capacity(list.len());
    for header in list {
        if header.name().starts_with(b":") {
            bail!("HTTP/3 trailer contains pseudo-header");
        }
        append_h3_header(&mut headers, header)?;
    }
    Ok(headers)
}

fn append_h3_header(headers: &mut HeaderMap, header: &h3::Header) -> Result<()> {
    let name = HeaderName::from_bytes(header.name()).context("invalid HTTP/3 header name")?;
    if name == CONNECTION
        || name == TRANSFER_ENCODING
        || name == UPGRADE
        || name.as_str() == "keep-alive"
        || name.as_str() == "proxy-connection"
    {
        bail!("HTTP/3 peer sent forbidden connection-specific header {name}");
    }
    let value = HeaderValue::from_bytes(header.value()).context("invalid HTTP/3 header value")?;
    headers.append(name, value);
    Ok(())
}

fn boxed_error(message: impl Into<String>) -> BoxError {
    Box::new(io::Error::other(message.into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_bodyless_get_and_head_are_early_data_safe() {
        for method in [Method::GET, Method::HEAD] {
            assert!(matches!(method, Method::GET | Method::HEAD));
        }
        assert!(!matches!(Method::POST, Method::GET | Method::HEAD));
    }

    #[test]
    fn strips_connection_specific_headers_from_h3_requests() {
        let mut headers = HeaderMap::new();
        headers.insert(HOST, HeaderValue::from_static("example.test"));
        headers.insert(CONNECTION, HeaderValue::from_static("close"));
        headers.insert("x-test", HeaderValue::from_static("ok"));
        let encoded = encode_regular_headers(&headers);
        assert!(encoded.iter().any(|header| header.name() == b"x-test"));
        assert!(!encoded.iter().any(|header| header.name() == b"connection"));
        assert!(!encoded.iter().any(|header| header.name() == b"host"));
    }

    #[test]
    fn reconnect_backoff_is_bounded_and_jittered() {
        assert_eq!(
            next_reconnect_delay(MIN_RECONNECT_DELAY),
            Duration::from_millis(200)
        );
        assert_eq!(
            next_reconnect_delay(MAX_RECONNECT_DELAY),
            MAX_RECONNECT_DELAY
        );
        assert_eq!(
            reconnect_delay_from_sample(Duration::from_secs(5), 0),
            Duration::from_secs(4)
        );
        assert_eq!(
            reconnect_delay_from_sample(Duration::from_secs(5), 40),
            Duration::from_secs(6)
        );
    }

    #[test]
    fn truncated_response_body_reports_an_error_and_cancels() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            use futures::StreamExt;

            let (commands, mut command_rx) = mpsc::channel(1);
            let (body_tx, body_rx) = mpsc::channel(1);
            drop(body_tx);
            let finished = Arc::new(AtomicBool::new(false));
            let cancellation = ResponseCancellation {
                id: 42,
                commands,
                finished: finished.clone(),
            };
            let body = response_body_stream(body_rx, finished, cancellation);
            futures::pin_mut!(body);

            let error = body.next().await.unwrap().unwrap_err();
            assert!(error.to_string().contains("before the stream finished"));
            assert!(body.next().await.is_none());
            assert!(matches!(
                command_rx.recv().await,
                Some(Command::Cancel { id: 42 })
            ));
        });
    }
}
