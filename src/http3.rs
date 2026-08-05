use std::convert::Infallible;
use std::error::Error as StdError;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
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
use tokio_quiche::quiche::h3::{self, NameValue};
use tokio_quiche::settings::{CertificateKind, Hooks, QuicSettings, TlsCertificatePaths};
use tokio_quiche::{ConnectionParams, ServerH3Driver, listen};

use crate::config::RuntimeConfig;

const INTERNAL_MARKER: &str = "x-jbs-http3-internal";
const INTERNAL_PORT: &str = "x-jbs-http3-port";
const STARTUP_TIMEOUT: Duration = Duration::from_secs(15);

type BoxError = Box<dyn StdError + Send + Sync>;
type ProxyBody = UnsyncBoxBody<Bytes, BoxError>;
type ProxyClient = Client<HttpConnector, ProxyBody>;

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

    let mut sockets = Vec::with_capacity(server.http3_listen.len());
    for address in &server.http3_listen {
        let socket = UdpSocket::bind(address)
            .await
            .with_context(|| format!("failed to bind HTTP/3 UDP listener {address}"))?;
        sockets.push(socket);
    }

    let mut quic = QuicSettings::default();
    quic.enable_dgram = false;
    quic.max_idle_timeout = Some(Duration::from_secs(server.http3_max_idle_timeout_seconds));
    quic.handshake_timeout = Some(Duration::from_secs(10));
    quic.listen_backlog = server.downstream_max_connections.min(16_384);
    quic.initial_max_streams_bidi = u64::from(server.http3_max_concurrent_streams);
    quic.enable_early_data = false;
    quic.disable_active_migration = true;
    quic.disable_client_ip_validation = false;

    let params = ConnectionParams::new_server(
        quic,
        TlsCertificatePaths {
            cert: certificate,
            private_key,
            kind: CertificateKind::X509,
        },
        Hooks::default(),
    );
    let listeners = listen(sockets, params, DefaultMetrics)
        .context("failed to create quiche HTTP/3 listeners")?;

    let mut connector = HttpConnector::new();
    connector.enforce_http(true);
    connector.set_connect_timeout(Some(Duration::from_secs(2)));
    let client: ProxyClient = Client::builder(TokioExecutor::new())
        .pool_max_idle_per_host(server.http3_max_concurrent_streams as usize)
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

    for mut listener in listeners {
        let client = client.clone();
        let alt_svc = alt_svc.clone();
        let internal_token = internal_token.clone();
        let connection_limit = connection_limit.clone();
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
                        let settings = Http3Settings {
                            max_header_list_size: Some(64 * 1024),
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
                            },
                            permit,
                        ));
                    }
                    Err(error) => warn!("HTTP/3 accept failed: {error}"),
                }
            }
        });
    }

    info!(
        "HTTP/3 frontend started: udp={:?} internal=http://{} quiche={} early_data=false migration=false",
        server.http3_listen,
        internal,
        tokio_quiche::quiche::PROTOCOL_VERSION,
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
}

async fn handle_connection(
    mut controller: ServerH3Controller,
    context: Http3ConnectionContext,
    _connection_permit: OwnedSemaphorePermit,
) {
    let peer = context.peer;
    while let Some(event) = controller.event_receiver_mut().recv().await {
        match event {
            ServerH3Event::Headers {
                incoming_headers,
                is_in_early_data,
                ..
            } => {
                if *is_in_early_data {
                    warn!("HTTP/3 early-data request rejected peer={peer}");
                    let IncomingH3Headers { mut send, .. } = incoming_headers;
                    if let Err(error) = send_error(
                        &mut send,
                        StatusCode::TOO_EARLY,
                        "HTTP/3 early data is not accepted",
                    )
                    .await
                    {
                        warn!("failed to reject HTTP/3 early-data request peer={peer}: {error:#}");
                    }
                    continue;
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

async fn proxy_request(incoming: IncomingH3Headers, context: Http3ConnectionContext) {
    let Http3ConnectionContext {
        peer,
        internal,
        public_port,
        internal_token,
        client,
        alt_svc,
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
        .version(Version::HTTP_11)
        .body(body)
        .context("failed to build internal HTTP/3 proxy request")?;
    *request.headers_mut() = decoded.headers;

    let response = tokio::time::timeout(Duration::from_secs(3600), client.request(request))
        .await
        .map_err(|_| anyhow!("internal Pingora request timed out"))?
        .context("internal Pingora request failed")?;
    forward_response(response, &mut send, alt_svc).await
}

struct DecodedRequest {
    method: Method,
    uri: Uri,
    headers: HeaderMap,
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
                    let frame = Frame::data(Bytes::copy_from_slice(data.as_ref()));
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
    }
}
