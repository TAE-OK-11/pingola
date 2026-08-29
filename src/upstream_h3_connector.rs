use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use cloudflare_pingora::connectors::http::custom::{self, Connection};
use cloudflare_pingora::http::{RequestHeader, ResponseHeader};
use cloudflare_pingora::protocols::Digest;
use cloudflare_pingora::protocols::http::custom::{BodyWrite, CustomMessageWrite};
use cloudflare_pingora::protocols::l4::socket::SocketAddr as PingoraAddr;
use cloudflare_pingora::upstreams::peer::Peer;
use cloudflare_pingora::{Error, ErrorType, Result};
use futures::Stream;
use http::header::CONTENT_LENGTH;
use http::{HeaderMap, Method};
use hyper::body::Frame;
use tokio::sync::{mpsc, oneshot};

use crate::upstream_h3::{
    Command, H3Pool, RequestHandle, ResponseCancellation, ResponseHead, UpstreamH3Registry,
    encode_pingora_request, send_body_command,
};

static NEXT_SESSION_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone)]
pub struct H3UpstreamConnector {
    registry: Arc<UpstreamH3Registry>,
}

impl H3UpstreamConnector {
    pub fn new(registry: Arc<UpstreamH3Registry>) -> Self {
        Self { registry }
    }

    fn pool_for_peer<P: Peer + Send + Sync>(&self, peer: &P) -> Result<Arc<H3Pool>> {
        let name = peer.sni();
        if name.is_empty() {
            return Err(Error::explain(
                ErrorType::ConnectError,
                "upstream HTTP/3 peer is missing upstream name",
            ));
        }
        self.registry.pool(name).ok_or_else(|| {
            Error::explain(
                ErrorType::ConnectError,
                format!("upstream HTTP/3 pool is unavailable: upstream={name}"),
            )
        })
    }
}

#[async_trait]
impl custom::Connector for H3UpstreamConnector {
    type Session = H3UpstreamSession;

    async fn get_http_session<P: Peer + Send + Sync + 'static>(
        &self,
        peer: &P,
    ) -> Result<(Connection<Self::Session>, bool)> {
        let pool = self.pool_for_peer(peer)?;
        let server_addr = peer.address().as_inet().copied().unwrap_or_else(|| {
            "0.0.0.0:0"
                .parse()
                .expect("placeholder socket address is valid")
        });
        let session = H3UpstreamSession::new(pool, server_addr);
        Ok((Connection::Session(session), false))
    }

    async fn reused_http_session<P: Peer + Send + Sync + 'static>(
        &self,
        _peer: &P,
    ) -> Option<Self::Session> {
        None
    }

    async fn release_http_session<P: Peer + Send + Sync + 'static>(
        &self,
        _session: Self::Session,
        _peer: &P,
        _idle_timeout: Option<Duration>,
    ) {
    }
}

pub struct H3UpstreamSession {
    pool: Arc<H3Pool>,
    server_addr: PingoraAddr,
    session_id: u64,
    handle: Option<RequestHandle>,
    response_header: Option<ResponseHeader>,
    body_rx: Option<mpsc::Receiver<Result<Frame<Bytes>, String>>>,
    body_buffer: Option<Bytes>,
    finished: Option<Arc<std::sync::atomic::AtomicBool>>,
    cancellation: Option<ResponseCancellation>,
    pending_trailers: Option<HeaderMap>,
    read_timeout: Option<Duration>,
    write_timeout: Option<Duration>,
    digest: Digest,
    body_done: bool,
    expects_request_body: bool,
}

impl H3UpstreamSession {
    fn new(pool: Arc<H3Pool>, server_addr: std::net::SocketAddr) -> Self {
        Self {
            pool,
            server_addr: PingoraAddr::Inet(server_addr),
            session_id: NEXT_SESSION_ID.fetch_add(1, Ordering::Relaxed),
            handle: None,
            response_header: None,
            body_rx: None,
            body_buffer: None,
            finished: None,
            cancellation: None,
            pending_trailers: None,
            read_timeout: None,
            write_timeout: None,
            digest: Digest::default(),
            body_done: false,
            expects_request_body: false,
        }
    }

    fn read_err(message: impl Into<String>) -> Box<Error> {
        Error::explain(ErrorType::ReadError, message.into())
    }

    fn write_err(message: impl Into<String>) -> Box<Error> {
        Error::explain(ErrorType::WriteError, message.into())
    }

    async fn recv_response_head(&mut self) -> Result<()> {
        let handle = self.handle.take().ok_or_else(|| {
            Self::read_err("upstream HTTP/3 response channel was already consumed")
        })?;
        let ResponseHead {
            status,
            headers,
            body,
            finished,
            cancellation,
        } = handle
            .response()
            .await
            .map_err(|error| Self::read_err(error.to_string()))?;
        let mut response =
            ResponseHeader::build_no_case(status, Some(headers.len())).map_err(|error| {
                Error::because(
                    ErrorType::InvalidHTTPHeader,
                    "upstream HTTP/3 response header build failed",
                    error,
                )
            })?;
        for (name, value) in &headers {
            response
                .insert_header(name.clone(), value.clone())
                .map_err(|error| {
                    Error::because(
                        ErrorType::InvalidHTTPHeader,
                        "upstream HTTP/3 response header insert failed",
                        error,
                    )
                })?;
        }
        self.response_header = Some(response);
        self.body_rx = Some(body);
        self.finished = Some(finished);
        self.cancellation = Some(cancellation);
        Ok(())
    }
}

struct H3RequestBodyWriter {
    id: u64,
    commands: mpsc::Sender<Command>,
    open: Option<oneshot::Receiver<Result<(), String>>>,
    expects_body: bool,
}

#[async_trait]
impl BodyWrite for H3RequestBodyWriter {
    async fn write_all_buf(&mut self, data: &mut Bytes) -> Result<()> {
        if let Some(open) = self.open.take() {
            open.await
                .map_err(|_| Self::write_err("upstream HTTP/3 request open channel closed"))?
                .map_err(Self::write_err)?;
        }
        if data.is_empty() {
            return Ok(());
        }
        send_body_command(&self.commands, self.id, data.clone(), false)
            .await
            .map_err(Self::write_err)?;
        data.clear();
        Ok(())
    }

    async fn finish(&mut self) -> Result<()> {
        if !self.expects_body {
            return Ok(());
        }
        if let Some(open) = self.open.take() {
            open.await
                .map_err(|_| Self::write_err("upstream HTTP/3 request open channel closed"))?
                .map_err(Self::write_err)?;
        }
        send_body_command(&self.commands, self.id, Bytes::new(), true)
            .await
            .map_err(Self::write_err)
    }

    async fn cleanup(&mut self) -> Result<()> {
        Ok(())
    }

    fn upgrade_body_writer(&mut self) {}
}

impl H3RequestBodyWriter {
    fn write_err(message: impl Into<String>) -> Box<Error> {
        Error::explain(ErrorType::WriteError, message.into())
    }
}

#[async_trait]
impl cloudflare_pingora::protocols::http::custom::client::Session for H3UpstreamSession {
    async fn write_request_header(&mut self, req: Box<RequestHeader>, end: bool) -> Result<()> {
        let has_body = !end;
        let allow_early_data = end && matches!(req.method, Method::GET | Method::HEAD);
        let headers =
            encode_pingora_request(&req).map_err(|error| Self::write_err(error.to_string()))?;
        let handle = self
            .pool
            .open(headers, has_body, allow_early_data)
            .await
            .map_err(|error| Self::write_err(error.to_string()))?;
        self.expects_request_body = has_body;
        self.handle = Some(handle);
        Ok(())
    }

    async fn write_request_body(&mut self, _data: Bytes, _end: bool) -> Result<()> {
        Err(Self::write_err(
            "upstream HTTP/3 request body must use the BodyWrite interface",
        ))
    }

    async fn finish_request_body(&mut self) -> Result<()> {
        Ok(())
    }

    fn set_read_timeout(&mut self, timeout: Option<Duration>) {
        self.read_timeout = timeout;
    }

    fn set_write_timeout(&mut self, timeout: Option<Duration>) {
        self.write_timeout = timeout;
    }

    async fn read_response_header(&mut self) -> Result<()> {
        if self.response_header.is_some() {
            return Ok(());
        }
        self.recv_response_head().await
    }

    async fn read_response_body(&mut self) -> Result<Option<Bytes>> {
        if self.body_done {
            return Ok(None);
        }
        if let Some(buffer) = self.body_buffer.take()
            && !buffer.is_empty()
        {
            return Ok(Some(buffer));
        }
        let body_rx = self
            .body_rx
            .as_mut()
            .ok_or_else(|| Self::read_err("upstream HTTP/3 response body is unavailable"))?;
        loop {
            match body_rx.recv().await {
                Some(Ok(frame)) => {
                    if let Some(data) = frame.data_ref()
                        && !data.is_empty()
                    {
                        return Ok(Some(data.clone()));
                    }
                    if let Some(trailers) = frame.trailers_ref() {
                        self.pending_trailers = Some(trailers.clone());
                        self.body_done = true;
                        return Ok(None);
                    }
                }
                Some(Err(error)) => {
                    return Err(Self::read_err(error));
                }
                None => {
                    if self
                        .finished
                        .as_ref()
                        .is_some_and(|finished| finished.load(Ordering::Acquire))
                    {
                        self.body_done = true;
                        return Ok(None);
                    }
                    return Err(Self::read_err(
                        "upstream HTTP/3 response ended before the stream finished",
                    ));
                }
            }
        }
    }

    fn response_finished(&self) -> bool {
        self.body_done
            || self
                .finished
                .as_ref()
                .is_some_and(|finished| finished.load(Ordering::Acquire))
    }

    async fn shutdown(&mut self, _code: u32, _ctx: &str) {}

    fn response_header(&self) -> Option<&ResponseHeader> {
        self.response_header.as_ref()
    }

    fn was_upgraded(&self) -> bool {
        false
    }

    fn digest(&self) -> Option<&Digest> {
        Some(&self.digest)
    }

    fn digest_mut(&mut self) -> Option<&mut Digest> {
        Some(&mut self.digest)
    }

    fn server_addr(&self) -> Option<&PingoraAddr> {
        Some(&self.server_addr)
    }

    fn client_addr(&self) -> Option<&PingoraAddr> {
        None
    }

    async fn read_trailers(&mut self) -> Result<Option<HeaderMap>> {
        Ok(self.pending_trailers.take())
    }

    fn fd(&self) -> cloudflare_pingora::protocols::UniqueIDType {
        self.session_id as cloudflare_pingora::protocols::UniqueIDType
    }

    async fn check_response_end_or_error(&mut self, headers: bool) -> Result<bool> {
        if headers {
            let Some(header) = self.response_header.as_ref() else {
                return Ok(false);
            };
            let status = header.status.as_u16();
            if status == 204 || status == 304 {
                return Ok(true);
            }
            if header
                .headers
                .get(CONTENT_LENGTH)
                .is_some_and(|value| value.as_bytes() == b"0")
            {
                return Ok(true);
            }
            return Ok(false);
        }
        Ok(self.response_finished())
    }

    fn take_request_body_writer(&mut self) -> Option<Box<dyn BodyWrite>> {
        let handle = self.handle.as_mut()?;
        Some(Box::new(H3RequestBodyWriter {
            id: handle.id,
            commands: handle.commands.clone(),
            open: handle.opened.take(),
            expects_body: self.expects_request_body,
        }))
    }

    async fn finish_custom(&mut self) -> Result<()> {
        Ok(())
    }

    fn take_custom_message_reader(
        &mut self,
    ) -> Option<Box<dyn Stream<Item = Result<Bytes>> + Unpin + Send + Sync + 'static>> {
        Some(Box::new(futures::stream::empty()))
    }

    async fn drain_custom_messages(&mut self) -> Result<()> {
        Ok(())
    }

    fn take_custom_message_writer(&mut self) -> Option<Box<dyn CustomMessageWrite>> {
        Some(Box::new(()))
    }
}
