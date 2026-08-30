use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::{BufMut, Bytes, BytesMut};
use cloudflare_pingora::http::{RequestHeader, ResponseHeader};
use cloudflare_pingora::protocols::Digest;
use cloudflare_pingora::protocols::http::HttpTask;
use cloudflare_pingora::protocols::http::custom::CustomMessageWrite;
use cloudflare_pingora::protocols::http::custom::server::Session as CustomSession;
use cloudflare_pingora::protocols::http::date::get_cached_date;
use cloudflare_pingora::protocols::l4::socket::SocketAddr as PingoraAddr;
use cloudflare_pingora::{Error, ErrorType, Result};
use futures::{SinkExt, Stream};
use http::header::{
    CONNECTION, CONTENT_LENGTH, DATE, HeaderName, TRAILER, TRANSFER_ENCODING, UPGRADE,
};
use http::{HeaderMap, HeaderValue};
use tokio_quiche::http3::driver::{
    InboundFrame, InboundFrameStream, OutboundFrame, OutboundFrameSender,
};
use tokio_quiche::quiche::h3;

const RETRY_BODY_LIMIT: usize = 64 * 1024;
const ALT_SVC: HeaderName = HeaderName::from_static("alt-svc");

pub struct H3Session {
    request_header: RequestHeader,
    send: OutboundFrameSender,
    recv: InboundFrameStream,
    request_fin: bool,
    ended: bool,
    body_read: usize,
    body_sent: usize,
    response_written: Option<Box<ResponseHeader>>,
    read_timeout: Option<Duration>,
    write_timeout: Option<Duration>,
    total_drain_timeout: Option<Duration>,
    retry_buffer: Option<BytesMut>,
    retry_truncated: bool,
    client_addr: PingoraAddr,
    server_addr: PingoraAddr,
    digest: Digest,
    alt_svc: Option<Arc<HeaderValue>>,
    wire_headers: Vec<h3::Header>,
}

impl H3Session {
    pub fn new(
        request_header: RequestHeader,
        send: OutboundFrameSender,
        recv: InboundFrameStream,
        request_fin: bool,
        client_addr: std::net::SocketAddr,
        server_addr: std::net::SocketAddr,
        alt_svc: Option<Arc<HeaderValue>>,
    ) -> Self {
        Self {
            request_header,
            send,
            recv,
            request_fin,
            ended: false,
            body_read: 0,
            body_sent: 0,
            response_written: None,
            read_timeout: None,
            write_timeout: None,
            total_drain_timeout: None,
            retry_buffer: None,
            retry_truncated: false,
            client_addr: PingoraAddr::Inet(client_addr),
            server_addr: PingoraAddr::Inet(server_addr),
            digest: Digest::default(),
            alt_svc,
            wire_headers: Vec::with_capacity(16),
        }
    }

    fn write_err(context: &'static str) -> Box<Error> {
        Error::explain(ErrorType::WriteError, context)
    }

    fn read_err(context: &'static str) -> Box<Error> {
        Error::explain(ErrorType::ReadError, context)
    }

    async fn send_frame(&mut self, frame: OutboundFrame) -> Result<()> {
        let send = self.send.send(frame);
        match self.write_timeout {
            Some(timeout) => tokio::time::timeout(timeout, send)
                .await
                .map_err(|_| Self::write_err("HTTP/3 response write timed out"))?
                .map_err(|_| Self::write_err("HTTP/3 response stream closed"))?,
            None => send
                .await
                .map_err(|_| Self::write_err("HTTP/3 response stream closed"))?,
        }
        Ok(())
    }

    fn hop_by_hop(name: &HeaderName) -> bool {
        name == CONNECTION
            || name == TRANSFER_ENCODING
            || name == UPGRADE
            || name == TRAILER
            || name.as_str() == "keep-alive"
            || name.as_str() == "proxy-connection"
    }

    fn encode_response_headers(&mut self, header: &ResponseHeader) {
        self.wire_headers.clear();
        self.wire_headers.push(h3::Header::new(
            b":status",
            header.status.as_str().as_bytes(),
        ));
        let date = get_cached_date();
        self.wire_headers
            .push(h3::Header::new(b"date", date.as_bytes()));
        if !header.headers.contains_key(&ALT_SVC)
            && let Some(value) = self.alt_svc.as_deref()
        {
            self.wire_headers
                .push(h3::Header::new(b"alt-svc", value.as_bytes()));
        }
        for (name, value) in &header.headers {
            if Self::hop_by_hop(name) || name == DATE || name == ALT_SVC {
                continue;
            }
            self.wire_headers
                .push(h3::Header::new(name.as_str().as_bytes(), value.as_bytes()));
        }
    }

    async fn send_headers_box(&mut self, mut header: Box<ResponseHeader>, end: bool) -> Result<()> {
        if self.ended {
            return Ok(());
        }
        if header.status.is_informational() {
            return Ok(());
        }
        if self.response_written.is_some() {
            return Ok(());
        }

        self.encode_response_headers(&header);
        let wire_headers = std::mem::take(&mut self.wire_headers);
        self.send_frame(OutboundFrame::Headers(wire_headers, None))
            .await?;
        if !header.headers.contains_key(&DATE) {
            header.insert_typed_header(DATE, get_cached_date());
        }
        self.response_written = Some(header);
        self.ended = end;
        if end {
            self.send_frame(OutboundFrame::Body(Bytes::new(), true))
                .await?;
        }
        Ok(())
    }

    async fn send_headers_ref(&mut self, header: &ResponseHeader, end: bool) -> Result<()> {
        if self.ended {
            return Ok(());
        }
        if header.status.is_informational() {
            return Ok(());
        }
        if self.response_written.is_some() {
            return Ok(());
        }

        self.encode_response_headers(header);
        let wire_headers = std::mem::take(&mut self.wire_headers);
        self.send_frame(OutboundFrame::Headers(wire_headers, None))
            .await?;
        self.response_written = Some(Box::new(
            ResponseHeader::build(header.status, None).unwrap(),
        ));
        self.ended = end;
        if end {
            self.send_frame(OutboundFrame::Body(Bytes::new(), true))
                .await?;
        }
        Ok(())
    }

    async fn send_body(&mut self, data: Bytes, end: bool) -> Result<()> {
        if self.ended {
            return Ok(());
        }
        if self.response_written.is_none() {
            return Err(Self::write_err("HTTP/3 response body sent before headers"));
        }
        if data.is_empty() && !end {
            return Ok(());
        }
        let len = data.len();
        self.send_frame(OutboundFrame::Body(data, end)).await?;
        self.body_sent += len;
        self.ended |= end;
        Ok(())
    }

    async fn apply_task(&mut self, task: HttpTask) -> Result<bool> {
        match task {
            HttpTask::Header(header, end) => {
                self.send_headers_box(header, end).await?;
                Ok(end)
            }
            HttpTask::Body(data, end) => {
                if let Some(data) = data
                    && !data.is_empty()
                {
                    self.send_body(data, end).await?;
                } else if end {
                    self.send_body(Bytes::new(), true).await?;
                }
                Ok(end)
            }
            HttpTask::UpgradedBody(..) => Err(Error::explain(
                ErrorType::InternalError,
                "upgraded body on HTTP/3 session",
            )),
            HttpTask::Trailer(Some(trailers)) => {
                self.write_trailers(*trailers).await?;
                Ok(true)
            }
            HttpTask::Trailer(None) | HttpTask::Done => Ok(true),
            HttpTask::Failed(error) => Err(error),
        }
    }
}

impl Drop for H3Session {
    fn drop(&mut self) {
        self.retry_buffer = None;
        self.wire_headers.clear();
        self.response_written = None;
    }
}

#[async_trait]
impl CustomSession for H3Session {
    fn req_header(&self) -> &RequestHeader {
        &self.request_header
    }

    fn req_header_mut(&mut self) -> &mut RequestHeader {
        &mut self.request_header
    }

    async fn read_body_bytes(&mut self) -> Result<Option<Bytes>> {
        if self.request_fin {
            return Ok(None);
        }
        loop {
            let recv = self.recv.recv();
            let frame = match self.read_timeout {
                Some(timeout) => tokio::time::timeout(timeout, recv)
                    .await
                    .map_err(|_| Self::read_err("HTTP/3 request body read timed out"))?,
                None => recv.await,
            };
            match frame {
                Some(InboundFrame::Body(data, fin)) => {
                    self.request_fin = fin;
                    if data.is_empty() {
                        if fin {
                            return Ok(None);
                        }
                        continue;
                    }
                    let body = data.freeze();
                    self.body_read += body.len();
                    if let Some(buffer) = self.retry_buffer.as_mut()
                        && !self.retry_truncated
                    {
                        if buffer.len().saturating_add(body.len()) > RETRY_BODY_LIMIT {
                            self.retry_truncated = true;
                            buffer.clear();
                        } else {
                            buffer.extend_from_slice(&body);
                        }
                    }
                    return Ok(Some(body));
                }
                Some(InboundFrame::Datagram(_)) => continue,
                None => {
                    self.request_fin = true;
                    return Ok(None);
                }
            }
        }
    }

    async fn drain_request_body(&mut self) -> Result<()> {
        let timeout = self.total_drain_timeout;
        let drain = async {
            while self.read_body_bytes().await?.is_some() {}
            Ok(())
        };
        match timeout {
            Some(timeout) => tokio::time::timeout(timeout, drain)
                .await
                .map_err(|_| Self::read_err("HTTP/3 request body drain timed out"))?,
            None => drain.await,
        }
    }

    async fn write_response_header(&mut self, resp: Box<ResponseHeader>, end: bool) -> Result<()> {
        self.send_headers_box(resp, end).await
    }

    async fn write_response_header_ref(&mut self, resp: &ResponseHeader, end: bool) -> Result<()> {
        self.send_headers_ref(resp, end).await
    }

    async fn write_body(&mut self, data: Bytes, end: bool) -> Result<()> {
        self.send_body(data, end).await
    }

    async fn write_trailers(&mut self, trailers: HeaderMap) -> Result<()> {
        if self.ended {
            return Ok(());
        }
        if self.response_written.is_none() {
            return Err(Self::write_err(
                "HTTP/3 trailers sent before response headers",
            ));
        }
        self.wire_headers.clear();
        for (name, value) in &trailers {
            if Self::hop_by_hop(name) {
                continue;
            }
            self.wire_headers
                .push(h3::Header::new(name.as_str().as_bytes(), value.as_bytes()));
        }
        if !self.wire_headers.is_empty() {
            let wire_headers = std::mem::take(&mut self.wire_headers);
            self.send_frame(OutboundFrame::Headers(wire_headers, None))
                .await?;
        }
        self.send_frame(OutboundFrame::Body(Bytes::new(), true))
            .await?;
        self.ended = true;
        Ok(())
    }

    async fn response_duplex_one(&mut self, task: HttpTask) -> Result<bool> {
        let end_stream = self.apply_task(task).await?;
        if end_stream {
            self.finish().await?;
        }
        Ok(end_stream)
    }

    async fn response_duplex_vec(&mut self, tasks: Vec<HttpTask>) -> Result<bool> {
        let mut end_stream = false;
        for task in tasks {
            end_stream = self.apply_task(task).await? || end_stream;
        }
        if end_stream {
            self.finish().await?;
        }
        Ok(end_stream)
    }

    fn set_read_timeout(&mut self, timeout: Option<Duration>) {
        self.read_timeout = timeout;
    }

    fn get_read_timeout(&self) -> Option<Duration> {
        self.read_timeout
    }

    fn set_write_timeout(&mut self, timeout: Option<Duration>) {
        self.write_timeout = timeout;
    }

    fn get_write_timeout(&self) -> Option<Duration> {
        self.write_timeout
    }

    fn set_total_drain_timeout(&mut self, timeout: Option<Duration>) {
        self.total_drain_timeout = timeout;
    }

    fn get_total_drain_timeout(&self) -> Option<Duration> {
        self.total_drain_timeout
    }

    fn request_summary(&self) -> String {
        format!(
            "{} {}, Host: {}",
            self.request_header.method,
            self.request_header
                .uri
                .path_and_query()
                .map(|value| value.as_str())
                .unwrap_or("/"),
            self.request_header
                .headers
                .get(http::header::HOST)
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default()
        )
    }

    fn response_written(&self) -> Option<&ResponseHeader> {
        self.response_written.as_deref()
    }

    async fn shutdown(&mut self, _code: u32, _ctx: &str) {
        if !self.ended {
            let _ = self
                .send_frame(OutboundFrame::Body(Bytes::new(), true))
                .await;
            self.ended = true;
        }
    }

    fn is_body_done(&mut self) -> bool {
        self.is_body_empty() || self.request_fin
    }

    async fn finish(&mut self) -> Result<()> {
        if self.ended || self.response_written.is_none() {
            return Ok(());
        }
        self.send_frame(OutboundFrame::Body(Bytes::new(), true))
            .await?;
        self.ended = true;
        Ok(())
    }

    fn is_body_empty(&mut self) -> bool {
        self.body_read == 0
            && (self.request_fin
                || self
                    .request_header
                    .headers
                    .get(CONTENT_LENGTH)
                    .is_some_and(|value| value.as_bytes() == b"0"))
    }

    async fn read_body_or_idle(&mut self, no_body_expected: bool) -> Result<Option<Bytes>> {
        if no_body_expected || self.is_body_done() {
            std::future::pending::<()>().await;
            Ok(None)
        } else {
            self.read_body_bytes().await
        }
    }

    fn body_bytes_sent(&self) -> usize {
        self.body_sent
    }

    fn body_bytes_read(&self) -> usize {
        self.body_read
    }

    fn digest(&self) -> Option<&Digest> {
        Some(&self.digest)
    }

    fn digest_mut(&mut self) -> Option<&mut Digest> {
        Some(&mut self.digest)
    }

    fn client_addr(&self) -> Option<&PingoraAddr> {
        Some(&self.client_addr)
    }

    fn server_addr(&self) -> Option<&PingoraAddr> {
        Some(&self.server_addr)
    }

    fn pseudo_raw_h1_request_header(&self) -> Bytes {
        let mut buf = BytesMut::with_capacity(256);
        buf.put_slice(self.request_header.method.as_str().as_bytes());
        buf.put_u8(b' ');
        buf.put_slice(
            self.request_header
                .uri
                .path_and_query()
                .map(|value| value.as_str().as_bytes())
                .unwrap_or(b"/"),
        );
        buf.put_slice(b" HTTP/1.1\r\n");
        self.request_header.header_to_h1_wire(&mut buf);
        buf.put_slice(b"\r\n");
        buf.freeze()
    }

    fn enable_retry_buffering(&mut self) {
        if self.retry_buffer.is_none() {
            self.retry_buffer = Some(BytesMut::new());
        }
    }

    fn retry_buffer_truncated(&self) -> bool {
        self.retry_truncated
    }

    fn get_retry_buffer(&self) -> Option<Bytes> {
        if self.retry_truncated {
            None
        } else {
            self.retry_buffer
                .as_ref()
                .filter(|buffer| !buffer.is_empty())
                .map(|buffer| buffer.clone().freeze())
        }
    }

    async fn finish_custom(&mut self) -> Result<()> {
        Ok(())
    }

    fn take_custom_message_reader(
        &mut self,
    ) -> Option<Box<dyn Stream<Item = Result<Bytes>> + Unpin + Send + Sync + 'static>> {
        None
    }

    fn restore_custom_message_reader(
        &mut self,
        _reader: Box<dyn Stream<Item = Result<Bytes>> + Unpin + Send + Sync + 'static>,
    ) -> Result<()> {
        Ok(())
    }

    fn take_custom_message_writer(&mut self) -> Option<Box<dyn CustomMessageWrite>> {
        None
    }

    fn restore_custom_message_writer(
        &mut self,
        _writer: Box<dyn CustomMessageWrite>,
    ) -> Result<()> {
        Ok(())
    }
}
