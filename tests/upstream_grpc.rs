use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use bytes::Bytes;
use http::header::{CONTENT_TYPE, TE};
use tempfile::TempDir;
use tokio::net::TcpListener as TokioTcpListener;

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn unused_address() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap()
}

fn wait_for_listener(address: SocketAddr) {
    for _ in 0..100 {
        if TcpStream::connect_timeout(&address, Duration::from_millis(50)).is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("proxy did not bind its HTTP listener");
}

fn http1_post(address: SocketAddr, host: &str, content_type: &str, body: &[u8]) -> String {
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(5)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let request = format!(
        "POST /pkg.Svc/Method HTTP/1.1\r\nHost: {host}\r\nContent-Type: {content_type}\r\nTE: trailers\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(request.as_bytes()).unwrap();
    stream.write_all(body).unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).unwrap();
    String::from_utf8_lossy(&response).into_owned()
}

fn write_config(proxy: SocketAddr, upstream: SocketAddr, runtime: &TempDir) -> std::path::PathBuf {
    let config = runtime.path().join("pingora.yaml");
    std::fs::write(
        &config,
        format!(
            r#"server:
  http_listen: ["{proxy}"]
  https_listen: []
  health_socket: "{health_socket}"
  threads: 1
  max_retries: 0
  upstream_keepalive_pool_size: 16
trusted_proxies: ["127.0.0.0/8"]
upstreams:
  backend:
    address: "{upstream}"
    protocol: grpc
    http2_max_concurrent_streams: 16
hosts:
  backend:
    domains: ["grpc.test"]
    handler: vaultwarden
    upstream: backend
route_limits:
  vaultwarden: {{ rate_per_second: 0, active_requests: 0 }}
  vaultwarden_hub: {{ rate_per_second: 0, active_requests: 0 }}
  vaultwarden_auth: {{ rate_per_second: 0, active_requests: 0 }}
"#,
            health_socket = runtime.path().join("health.sock").display(),
        ),
    )
    .unwrap();
    config
}

fn spawn_proxy(config: &std::path::Path) -> ChildGuard {
    let child = Command::new(env!("CARGO_BIN_EXE_pingora"))
        .arg("--config")
        .arg(config)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    ChildGuard(child)
}

const EMPTY_GRPC_FRAME: &[u8] = &[0, 0, 0, 0, 0];

async fn grpc_origin(
    listener: TokioTcpListener,
    expect_web_converted: bool,
    saw_te: Arc<AtomicBool>,
    saw_grpc: Arc<AtomicBool>,
    requests: Arc<AtomicUsize>,
) {
    loop {
        let (stream, _) = listener.accept().await.unwrap();
        let saw_te = Arc::clone(&saw_te);
        let saw_grpc = Arc::clone(&saw_grpc);
        let requests = Arc::clone(&requests);
        tokio::spawn(async move {
            let mut connection = h2::server::handshake(stream).await.unwrap();
            while let Some(result) = connection.accept().await {
                let (request, mut respond) = result.unwrap();
                assert_eq!(request.version(), http::Version::HTTP_2);
                let content_type = request
                    .headers()
                    .get(CONTENT_TYPE)
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or_default();
                if expect_web_converted {
                    assert_eq!(content_type, "application/grpc+proto");
                } else {
                    assert_eq!(content_type, "application/grpc");
                }
                saw_grpc.store(true, Ordering::Relaxed);
                let te = request
                    .headers()
                    .get(TE)
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or_default();
                if te.eq_ignore_ascii_case("trailers") {
                    saw_te.store(true, Ordering::Relaxed);
                }
                requests.fetch_add(1, Ordering::Relaxed);

                let response_type = if expect_web_converted {
                    "application/grpc+proto"
                } else {
                    "application/grpc"
                };
                let response = http::Response::builder()
                    .status(200)
                    .header(CONTENT_TYPE, response_type)
                    .body(())
                    .unwrap();
                let mut body = respond.send_response(response, false).unwrap();
                body.send_data(Bytes::from_static(EMPTY_GRPC_FRAME), false)
                    .unwrap();
                let mut trailers = http::HeaderMap::new();
                trailers.insert("grpc-status", http::HeaderValue::from_static("0"));
                trailers.insert("grpc-message", http::HeaderValue::from_static("OK"));
                body.send_trailers(trailers).unwrap();
            }
        });
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn grpc_web_is_bridged_to_h2_grpc_with_trailers() {
    let upstream = TokioTcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_address = upstream.local_addr().unwrap();
    let saw_te = Arc::new(AtomicBool::new(false));
    let saw_grpc = Arc::new(AtomicBool::new(false));
    let requests = Arc::new(AtomicUsize::new(0));
    let backend = tokio::spawn(grpc_origin(
        upstream,
        true,
        Arc::clone(&saw_te),
        Arc::clone(&saw_grpc),
        Arc::clone(&requests),
    ));

    let proxy_address = unused_address();
    let runtime = TempDir::new().unwrap();
    let config = write_config(proxy_address, upstream_address, &runtime);
    let _guard = spawn_proxy(&config);
    wait_for_listener(proxy_address);

    let response = http1_post(
        proxy_address,
        "grpc.test",
        "application/grpc-web+proto",
        EMPTY_GRPC_FRAME,
    );
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    let lower = response.to_ascii_lowercase();
    assert!(
        lower.contains("content-type: application/grpc-web+proto"),
        "{response}"
    );
    assert!(response.contains("grpc-status:0"), "{response}");
    assert!(response.contains("grpc-message:OK"), "{response}");
    assert_eq!(requests.load(Ordering::Relaxed), 1);
    assert!(saw_te.load(Ordering::Relaxed));
    assert!(saw_grpc.load(Ordering::Relaxed));
    backend.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_grpc_forwards_te_trailers_to_h2_origin() {
    let upstream = TokioTcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_address = upstream.local_addr().unwrap();
    let saw_te = Arc::new(AtomicBool::new(false));
    let saw_grpc = Arc::new(AtomicBool::new(false));
    let requests = Arc::new(AtomicUsize::new(0));
    let backend = tokio::spawn(grpc_origin(
        upstream,
        false,
        Arc::clone(&saw_te),
        Arc::clone(&saw_grpc),
        Arc::clone(&requests),
    ));

    let proxy_address = unused_address();
    let runtime = TempDir::new().unwrap();
    let config = write_config(proxy_address, upstream_address, &runtime);
    let _guard = spawn_proxy(&config);
    wait_for_listener(proxy_address);

    let response = http1_post(
        proxy_address,
        "grpc.test",
        "application/grpc",
        EMPTY_GRPC_FRAME,
    );
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    let lower = response.to_ascii_lowercase();
    assert!(
        lower.contains("content-type: application/grpc"),
        "{response}"
    );
    assert!(!lower.contains("application/grpc-web"), "{response}");
    assert_eq!(requests.load(Ordering::Relaxed), 1);
    assert!(saw_te.load(Ordering::Relaxed));
    assert!(saw_grpc.load(Ordering::Relaxed));
    backend.abort();
}
