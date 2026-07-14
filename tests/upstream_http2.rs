use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
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

fn proxy_get(address: SocketAddr) -> String {
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(5)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream
        .write_all(b"GET / HTTP/1.1\r\nHost: h2.test\r\nConnection: close\r\n\r\n")
        .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    response
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explicit_h2c_upstream_is_used_and_reused() {
    let upstream = TokioTcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_address = upstream.local_addr().unwrap();
    let connections = Arc::new(AtomicUsize::new(0));
    let requests = Arc::new(AtomicUsize::new(0));
    let connections_task = Arc::clone(&connections);
    let requests_task = Arc::clone(&requests);

    let backend = tokio::spawn(async move {
        loop {
            let (stream, _) = upstream.accept().await.unwrap();
            connections_task.fetch_add(1, Ordering::Relaxed);
            let requests = Arc::clone(&requests_task);
            tokio::spawn(async move {
                let mut connection = h2::server::handshake(stream).await.unwrap();
                while let Some(result) = connection.accept().await {
                    let (request, mut respond) = result.unwrap();
                    assert_eq!(request.version(), http::Version::HTTP_2);
                    requests.fetch_add(1, Ordering::Relaxed);
                    let response = http::Response::builder()
                        .status(200)
                        .header(http::header::CONTENT_LENGTH, "11")
                        .header("x-upstream-protocol", "h2")
                        .body(())
                        .unwrap();
                    let mut body = respond.send_response(response, false).unwrap();
                    body.send_data(Bytes::from_static(b"h2-upstream"), true)
                        .unwrap();
                }
            });
        }
    });

    let proxy_address = unused_address();
    let runtime = TempDir::new().unwrap();
    let config = runtime.path().join("pingora.yaml");
    std::fs::write(
        &config,
        format!(
            r#"server:
  http_listen: ["{proxy_address}"]
  https_listen: []
  health_socket: "{health_socket}"
  threads: 1
  max_retries: 0
  upstream_keepalive_pool_size: 16
trusted_proxies: ["127.0.0.0/8"]
upstreams:
  backend:
    address: "{upstream_address}"
    protocol: http2
    http2_max_concurrent_streams: 16
hosts:
  backend:
    domains: ["h2.test"]
    handler: vaultwarden
    upstream: backend
route_limits:
  vaultwarden: {{ rate_per_second: 0, active_requests: 0 }}
"#,
            health_socket = runtime.path().join("health.sock").display(),
        ),
    )
    .unwrap();

    let child = Command::new(env!("CARGO_BIN_EXE_pingora"))
        .arg("--config")
        .arg(&config)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let _guard = ChildGuard(child);

    let mut ready = false;
    for _ in 0..100 {
        if TcpStream::connect_timeout(&proxy_address, Duration::from_millis(50)).is_ok() {
            ready = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(ready, "proxy did not bind its HTTP listener");

    for _ in 0..3 {
        let response = proxy_get(proxy_address);
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert!(response
            .to_ascii_lowercase()
            .contains("x-upstream-protocol: h2"));
        assert!(response.ends_with("h2-upstream"));
    }

    assert_eq!(requests.load(Ordering::Relaxed), 3);
    assert_eq!(connections.load(Ordering::Relaxed), 1);
    backend.abort();
}
