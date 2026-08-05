#!/usr/bin/env python3
from pathlib import Path


def replace(path: str, old: str, new: str, count: int = 1) -> None:
    file = Path(path)
    text = file.read_text()
    actual = text.count(old)
    if actual != count:
        raise SystemExit(f"{path}: expected {count} matches, found {actual}: {old!r}")
    file.write_text(text.replace(old, new))


replace(
    "src/http3.rs",
    "use tokio::net::UdpSocket;",
    "use tokio::net::UdpSocket;\nuse tokio::sync::{OwnedSemaphorePermit, Semaphore};",
)
replace(
    "src/http3.rs",
    '''    let internal = server.http3_internal_listen;
    let alt_svc = runtime.http3_alt_svc_header();

    for mut listener in listeners {
        let client = client.clone();
        let alt_svc = alt_svc.clone();''',
    '''    let internal = server.http3_internal_listen;
    let public_port = runtime
        .http3_public_port()
        .ok_or_else(|| anyhow!("HTTP/3 public port was not configured"))?;
    let alt_svc = runtime.http3_alt_svc_header();
    let connection_limit = Arc::new(Semaphore::new(server.downstream_max_connections));

    for mut listener in listeners {
        let client = client.clone();
        let alt_svc = alt_svc.clone();
        let connection_limit = connection_limit.clone();''',
)
replace(
    "src/http3.rs",
    '''                    Ok(connection) => {
                        let peer = connection.peer_addr();
                        let (driver, controller) = ServerH3Driver::new(Http3Settings::default());
                        connection.start(driver);
                        tokio::spawn(handle_connection(
                            controller,
                            peer,
                            internal,
                            client.clone(),
                            alt_svc.clone(),
                        ));
                    }''',
    '''                    Ok(connection) => {
                        let permit = match connection_limit.clone().try_acquire_owned() {
                            Ok(permit) => permit,
                            Err(_) => {
                                warn!("HTTP/3 connection rejected: downstream connection limit reached");
                                continue;
                            }
                        };
                        let peer = connection.peer_addr();
                        let mut settings = Http3Settings::default();
                        settings.max_header_list_size = Some(64 * 1024);
                        let (driver, controller) = ServerH3Driver::new(settings);
                        connection.start(driver);
                        tokio::spawn(handle_connection(
                            controller,
                            peer,
                            internal,
                            public_port,
                            client.clone(),
                            alt_svc.clone(),
                            permit,
                        ));
                    }''',
)
replace(
    "src/http3.rs",
    '''async fn handle_connection(
    mut controller: ServerH3Controller,
    peer: SocketAddr,
    internal: SocketAddr,
    client: ProxyClient,
    alt_svc: Option<HeaderValue>,
) {''',
    '''async fn handle_connection(
    mut controller: ServerH3Controller,
    peer: SocketAddr,
    internal: SocketAddr,
    public_port: u16,
    client: ProxyClient,
    alt_svc: Option<HeaderValue>,
    _connection_permit: OwnedSemaphorePermit,
) {''',
)
replace(
    "src/http3.rs",
    '''                    peer,
                    internal,
                    client.clone(),''',
    '''                    peer,
                    internal,
                    public_port,
                    client.clone(),''',
)
replace(
    "src/http3.rs",
    '''async fn proxy_request(
    incoming: IncomingH3Headers,
    peer: SocketAddr,
    internal: SocketAddr,
    client: ProxyClient,
    alt_svc: Option<HeaderValue>,
) {''',
    '''async fn proxy_request(
    incoming: IncomingH3Headers,
    peer: SocketAddr,
    internal: SocketAddr,
    public_port: u16,
    client: ProxyClient,
    alt_svc: Option<HeaderValue>,
) {''',
)
replace(
    "src/http3.rs",
    '''        headers, send, recv, read_fin, peer, internal, client, alt_svc,
''',
    '''        headers,
        send,
        recv,
        read_fin,
        peer,
        internal,
        public_port,
        client,
        alt_svc,
''',
)
replace(
    "src/http3.rs",
    '''    peer: SocketAddr,
    internal: SocketAddr,
    client: ProxyClient,
    alt_svc: Option<HeaderValue>,
) -> Result<()> {
    let decoded = match decode_request_headers(&headers, peer, internal) {''',
    '''    peer: SocketAddr,
    internal: SocketAddr,
    public_port: u16,
    client: ProxyClient,
    alt_svc: Option<HeaderValue>,
) -> Result<()> {
    let decoded = match decode_request_headers(&headers, peer, internal, public_port) {''',
)
replace(
    "src/http3.rs",
    '''    peer: SocketAddr,
    internal: SocketAddr,
) -> Result<DecodedRequest> {''',
    '''    peer: SocketAddr,
    internal: SocketAddr,
    public_port: u16,
) -> Result<DecodedRequest> {''',
)
replace(
    "src/http3.rs",
    '''    output.insert(
        "x-forwarded-port",
        HeaderValue::from_str(&internal.port().to_string())
            .context("invalid HTTP/3 port header")?,
    );''',
    '''    let public_port = HeaderValue::from_str(&public_port.to_string())
        .context("invalid HTTP/3 public port header")?;
    output.insert("x-forwarded-port", public_port.clone());''',
)
replace(
    "src/http3.rs",
    '''    output.insert(
        INTERNAL_PORT,
        HeaderValue::from_str("443").expect("static HTTP/3 port is valid"),
    );''',
    '''    output.insert(INTERNAL_PORT, public_port);''',
)
replace(
    "src/http3.rs",
    '''                "127.0.0.1:18080".parse().unwrap(),
            )''',
    '''                "127.0.0.1:18080".parse().unwrap(),
                443,
            )''',
)
replace(
    "src/http3.rs",
    '''            "127.0.0.1:18080".parse().unwrap(),
        )''',
    '''            "127.0.0.1:18080".parse().unwrap(),
            8443,
        )''',
)
replace(
    "src/http3.rs",
    '''        assert_eq!(request.headers[INTERNAL_MARKER], "1");
        assert_eq!(request.headers["x-forwarded-for"], "192.0.2.10");''',
    '''        assert_eq!(request.headers[INTERNAL_MARKER], "1");
        assert_eq!(request.headers["x-forwarded-for"], "192.0.2.10");
        assert_eq!(request.headers["x-forwarded-port"], "8443");
        assert_eq!(request.headers[INTERNAL_PORT], "8443");''',
)
