use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use tokio::net::UdpSocket;
use tokio_quiche::http3::driver::{
    ClientH3Event, H3Event, InboundFrame, IncomingH3Headers, NewClientRequest,
};
use tokio_quiche::quiche::h3::{self, NameValue};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_REQUESTS: u64 = 100_000;
const MAX_CONCURRENCY: u64 = 256;

#[tokio::main]
async fn main() -> Result<()> {
    let mut arguments = std::env::args().skip(1);
    let peer: SocketAddr = arguments
        .next()
        .ok_or_else(|| {
            anyhow!("usage: http3_probe <address> <authority> <path> [requests] [concurrency] [accept-encoding]")
        })?
        .parse()
        .context("invalid HTTP/3 peer address")?;
    let authority = arguments
        .next()
        .ok_or_else(|| anyhow!("missing HTTP/3 authority"))?;
    let path = arguments
        .next()
        .ok_or_else(|| anyhow!("missing HTTP/3 path"))?;
    let requests = arguments
        .next()
        .map(|value| value.parse::<u64>().context("invalid HTTP/3 request count"))
        .transpose()?
        .unwrap_or(1);
    let concurrency = arguments
        .next()
        .map(|value| value.parse::<u64>().context("invalid HTTP/3 concurrency"))
        .transpose()?
        .unwrap_or(1);
    let accept_encoding = arguments.next();
    if arguments.next().is_some() {
        bail!("too many arguments");
    }
    if !path.starts_with('/') {
        bail!("HTTP/3 path must start with '/'");
    }
    if !(1..=MAX_REQUESTS).contains(&requests) {
        bail!("HTTP/3 request count must be between 1 and {MAX_REQUESTS}");
    }
    if !(1..=MAX_CONCURRENCY).contains(&concurrency) {
        bail!("HTTP/3 concurrency must be between 1 and {MAX_CONCURRENCY}");
    }
    let concurrency = concurrency.min(requests);

    let bind_address = SocketAddr::new(
        match peer.ip() {
            IpAddr::V4(_) => IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
            IpAddr::V6(_) => IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED),
        },
        0,
    );
    let socket = UdpSocket::bind(bind_address)
        .await
        .context("failed to bind HTTP/3 client UDP socket")?;
    socket
        .connect(peer)
        .await
        .with_context(|| format!("failed to connect HTTP/3 UDP socket to {peer}"))?;

    let (_, mut controller) = tokio_quiche::quic::connect(socket, Some(&authority))
        .await
        .map_err(|error| anyhow!("HTTP/3 QUIC handshake failed: {error}"))?;

    let expected_alt_svc = format!("h3=\":{}\"", peer.port());
    let mut request_headers = vec![
        h3::Header::new(b":method", b"GET"),
        h3::Header::new(b":scheme", b"https"),
        h3::Header::new(b":authority", authority.as_bytes()),
        h3::Header::new(b":path", path.as_bytes()),
        h3::Header::new(b"user-agent", b"jbs-http3-probe/2"),
        h3::Header::new(b"accept", b"application/json, text/html;q=0.9"),
    ];
    if let Some(value) = accept_encoding {
        request_headers.push(h3::Header::new(b"accept-encoding", value.as_bytes()));
    }

    let mut next_request_id = 1_u64;
    let mut completed = 0_u64;
    let mut stream_requests: HashMap<u64, u64> = HashMap::new();
    let mut single_body = None;

    let send_one = |controller: &mut tokio_quiche::http3::driver::ClientH3Controller,
                    request_id: u64,
                    headers: &[h3::Header]|
     -> Result<()> {
        controller
            .request_sender()
            .send(NewClientRequest {
                request_id,
                headers: headers.to_vec(),
                body_writer: None,
            })
            .map_err(|_| anyhow!("failed to queue HTTP/3 request: controller is closed"))
    };

    while next_request_id <= concurrency {
        send_one(&mut controller, next_request_id, &request_headers)?;
        next_request_id += 1;
    }

    while completed < requests {
        let event = tokio::time::timeout(REQUEST_TIMEOUT, controller.event_receiver_mut().recv())
            .await
            .with_context(|| {
                format!("HTTP/3 response {completed}/{requests} timed out waiting for an event")
            })?
            .ok_or_else(|| anyhow!("HTTP/3 connection closed before all responses arrived"))?;
        match event {
            ClientH3Event::NewOutboundRequest {
                request_id,
                stream_id,
            } => {
                stream_requests.insert(stream_id, request_id);
            }
            ClientH3Event::Core(H3Event::IncomingHeaders(IncomingH3Headers {
                stream_id,
                headers,
                mut recv,
                read_fin,
                ..
            })) => {
                let request_id = stream_requests.remove(&stream_id).unwrap_or(stream_id);
                let response = collect_response(headers, &mut recv, read_fin, stream_id).await?;
                if response.status != 200 {
                    bail!(
                        "HTTP/3 server returned status {} for request {request_id}/{requests}",
                        response.status
                    );
                }
                if !response
                    .alt_svc
                    .as_deref()
                    .is_some_and(|value| value.contains(&expected_alt_svc))
                {
                    bail!(
                        "HTTP/3 response did not advertise expected Alt-Svc {expected_alt_svc:?}: {:?}",
                        response.alt_svc
                    );
                }
                if requests == 1 {
                    single_body = Some(response.body);
                }
                completed += 1;
                if next_request_id <= requests {
                    send_one(&mut controller, next_request_id, &request_headers)?;
                    next_request_id += 1;
                }
            }
            ClientH3Event::Core(H3Event::BodyBytesReceived { .. }) => {}
            ClientH3Event::Core(event) => {
                eprintln!("HTTP/3 probe event: {event:?}");
            }
        }
    }

    if let Some(body) = single_body {
        match String::from_utf8(body) {
            Ok(text) => print!("{}", text),
            Err(err) => eprintln!(
                "HTTP/3 probe completed 1 request with {} byte binary body",
                err.into_bytes().len()
            ),
        }
    } else {
        eprintln!(
            "HTTP/3 probe completed {requests} requests over one QUIC connection concurrency={concurrency}"
        );
    }
    Ok(())
}

struct ProbeResponse {
    status: u16,
    alt_svc: Option<String>,
    body: Vec<u8>,
}

async fn collect_response(
    headers: Vec<h3::Header>,
    recv: &mut tokio_quiche::http3::driver::InboundFrameStream,
    read_fin: bool,
    stream_id: u64,
) -> Result<ProbeResponse> {
    let mut status = None;
    let mut alt_svc = None;
    for header in headers {
        match header.name() {
            b":status" => {
                status = Some(
                    std::str::from_utf8(header.value())
                        .context("HTTP/3 :status is not UTF-8")?
                        .parse::<u16>()
                        .context("HTTP/3 :status is invalid")?,
                );
            }
            b"alt-svc" => {
                alt_svc = Some(
                    std::str::from_utf8(header.value())
                        .context("HTTP/3 Alt-Svc is not UTF-8")?
                        .to_string(),
                );
            }
            _ => {}
        }
    }
    let status = status.ok_or_else(|| anyhow!("HTTP/3 response lacks :status"))?;
    let mut body = Vec::new();
    if !read_fin {
        while let Some(frame) = recv.recv().await {
            match frame {
                InboundFrame::Body(data, fin) => {
                    body.extend_from_slice(data.as_ref());
                    if fin {
                        break;
                    }
                }
                InboundFrame::Datagram(_) => {
                    bail!("unexpected HTTP/3 DATAGRAM on response stream {stream_id}")
                }
            }
        }
    }
    Ok(ProbeResponse {
        status,
        alt_svc,
        body,
    })
}
