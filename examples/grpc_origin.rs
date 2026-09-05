//! Minimal plaintext HTTP/2 gRPC origin for PGO training and local checks.
//!
//! Speaks prior-knowledge h2c. Native `application/grpc*` requests get an
//! empty DATA frame plus `grpc-status` trailers. `application/grpc-web*` is
//! accepted so Pingora's bridge can be trained end-to-end.

use std::convert::Infallible;
use std::net::SocketAddr;

use anyhow::{Context as _, Result, anyhow};
use bytes::Bytes;
use futures::stream;
use http::header::CONTENT_TYPE;
use http::{HeaderMap, HeaderValue, Request, Response};
use http_body_util::{BodyExt, StreamBody};
use hyper::body::{Frame, Incoming};
use hyper::server::conn::http2;
use hyper::service::service_fn;
use hyper_util::rt::{TokioExecutor, TokioIo};
use tokio::net::TcpListener;

const EMPTY_GRPC_FRAME: Bytes = Bytes::from_static(&[0, 0, 0, 0, 0]);

type GrpcBody = StreamBody<stream::Iter<std::array::IntoIter<Result<Frame<Bytes>, Infallible>, 2>>>;

fn grpc_response(content_type: &str) -> Response<GrpcBody> {
    let mut trailers = HeaderMap::new();
    trailers.insert("grpc-status", HeaderValue::from_static("0"));
    trailers.insert("grpc-message", HeaderValue::from_static("OK"));
    let frames = [
        Ok(Frame::data(EMPTY_GRPC_FRAME.clone())),
        Ok(Frame::trailers(trailers)),
    ];
    let mut response = Response::new(StreamBody::new(stream::iter(frames)));
    response.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_str(content_type)
            .unwrap_or_else(|_| HeaderValue::from_static("application/grpc")),
    );
    *response.status_mut() = http::StatusCode::OK;
    response
}

async fn handle(request: Request<Incoming>) -> Result<Response<GrpcBody>, Infallible> {
    let content_type = request
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("application/grpc")
        .to_owned();
    let _ = request.into_body().collect().await;
    Ok(grpc_response(&content_type))
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let mut arguments = std::env::args().skip(1);
    let listen: SocketAddr = arguments
        .next()
        .ok_or_else(|| anyhow!("usage: grpc_origin <listen-address>"))?
        .parse()
        .context("invalid gRPC origin listen address")?;
    if arguments.next().is_some() {
        anyhow::bail!("too many arguments");
    }

    let listener = TcpListener::bind(listen)
        .await
        .with_context(|| format!("failed to bind gRPC origin on {listen}"))?;
    eprintln!("gRPC origin listening on {listen}");

    loop {
        let (stream, _) = listener.accept().await?;
        tokio::spawn(async move {
            let io = TokioIo::new(stream);
            let service = service_fn(handle);
            if let Err(error) = http2::Builder::new(TokioExecutor::new())
                .serve_connection(io, service)
                .await
            {
                eprintln!("gRPC origin connection failed: {error}");
            }
        });
    }
}
