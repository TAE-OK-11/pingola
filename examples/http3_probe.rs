use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use tokio::net::UdpSocket;
use tokio_quiche::http3::driver::{
    ClientH3Event, H3Event, InboundFrame, IncomingH3Headers, NewClientRequest,
};
use tokio_quiche::quiche::h3::{self, NameValue};

const REQUEST_ID: u64 = 1;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

#[tokio::main]
async fn main() -> Result<()> {
    let mut arguments = std::env::args().skip(1);
    let peer: SocketAddr = arguments
        .next()
        .ok_or_else(|| anyhow!("usage: http3_probe <address> <authority> <path>"))?
        .parse()
        .context("invalid HTTP/3 peer address")?;
    let authority = arguments
        .next()
        .ok_or_else(|| anyhow!("missing HTTP/3 authority"))?;
    let path = arguments
        .next()
        .ok_or_else(|| anyhow!("missing HTTP/3 path"))?;
    if arguments.next().is_some() {
        bail!("too many arguments");
    }
    if !path.starts_with('/') {
        bail!("HTTP/3 path must start with '/'");
    }

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
    controller
        .request_sender()
        .send(NewClientRequest {
            request_id: REQUEST_ID,
            headers: vec![
                h3::Header::new(b":method", b"GET"),
                h3::Header::new(b":scheme", b"https"),
                h3::Header::new(b":authority", authority.as_bytes()),
                h3::Header::new(b":path", path.as_bytes()),
                h3::Header::new(b"user-agent", b"jbs-http3-probe/1"),
                h3::Header::new(b"accept", b"application/json, text/html;q=0.9"),
            ],
            body_writer: None,
        })
        .map_err(|_| anyhow!("failed to queue HTTP/3 request: controller is closed"))?;

    let response = tokio::time::timeout(REQUEST_TIMEOUT, receive_response(&mut controller))
        .await
        .context("HTTP/3 response timed out")??;
    if response.status != 200 {
        bail!("HTTP/3 server returned status {}", response.status);
    }
    let expected_alt_svc = format!(r#"h3=\":{}\""#, peer.port());
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

    print!(
        "{}",
        String::from_utf8(response.body).context("HTTP/3 body is not UTF-8")?
    );
    Ok(())
}

struct ProbeResponse {
    status: u16,
    alt_svc: Option<String>,
    body: Vec<u8>,
}

async fn receive_response(
    controller: &mut tokio_quiche::http3::driver::ClientH3Controller,
) -> Result<ProbeResponse> {
    while let Some(event) = controller.event_receiver_mut().recv().await {
        match event {
            ClientH3Event::Core(H3Event::IncomingHeaders(IncomingH3Headers {
                stream_id,
                headers,
                mut recv,
                read_fin,
                ..
            })) => {
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
                return Ok(ProbeResponse {
                    status,
                    alt_svc,
                    body,
                });
            }
            ClientH3Event::Core(H3Event::BodyBytesReceived { .. }) => {}
            ClientH3Event::Core(event) => {
                eprintln!("HTTP/3 probe event: {event:?}");
            }
            ClientH3Event::NewOutboundRequest {
                request_id,
                stream_id,
            } => {
                if request_id != REQUEST_ID {
                    bail!("unexpected HTTP/3 request id {request_id} on stream {stream_id}");
                }
            }
        }
    }
    bail!("HTTP/3 connection closed before a response arrived")
}
