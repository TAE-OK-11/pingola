use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

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

fn closed_by_peer(stream: &mut TcpStream) -> bool {
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut byte = [0_u8; 1];
    match stream.read(&mut byte) {
        Ok(0) => true,
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::ConnectionAborted
                    | std::io::ErrorKind::BrokenPipe
            ) =>
        {
            true
        }
        _ => false,
    }
}

#[test]
fn connection_cap_and_total_header_deadline_close_incomplete_clients() {
    let runtime = tempfile::tempdir().unwrap();
    let address = unused_address();
    let health_socket = runtime.path().join("health.sock");
    let config = runtime.path().join("pingora.yaml");
    std::fs::write(
        &config,
        format!(
            r#"
server:
  http_listen: ["{address}"]
  https_listen: []
  health_socket: {}
  threads: 1
  downstream_max_connections: 1
  downstream_request_header_timeout_seconds: 1
  graceful_shutdown_timeout_seconds: 1
trusted_proxies: ["127.0.0.0/8"]
upstreams:
  app:
    address: "127.0.0.1:9"
hosts:
  app:
    domains: ["app.test"]
    handler: vaultwarden
    upstream: app
"#,
            health_socket.display()
        ),
    )
    .unwrap();

    let mut child = ChildGuard(
        Command::new(env!("CARGO_BIN_EXE_pingora"))
            .args(["--config", config.to_str().unwrap()])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while !health_socket.exists() {
        assert!(
            Instant::now() < deadline,
            "proxy did not create health socket"
        );
        assert!(child.0.try_wait().unwrap().is_none(), "proxy exited early");
        std::thread::sleep(Duration::from_millis(20));
    }

    let mut first = TcpStream::connect(address).unwrap();
    first.write_all(b"G").unwrap();
    std::thread::sleep(Duration::from_millis(100));

    let mut excess = TcpStream::connect(address).unwrap();
    assert!(
        closed_by_peer(&mut excess),
        "excess connection stayed admitted"
    );

    std::thread::sleep(Duration::from_millis(1100));
    assert!(
        closed_by_peer(&mut first),
        "incomplete header outlived the total deadline"
    );
}
