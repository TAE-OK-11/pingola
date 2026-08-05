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
    '''                        tokio::spawn(handle_connection(
                            controller,
                            peer,
                            internal,
                            public_port,
                            internal_token.clone(),
                            client.clone(),
                            alt_svc.clone(),
                            permit,
                        ));''',
    '''                        tokio::spawn(handle_connection(
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
                        ));''',
)
replace(
    "src/http3.rs",
    '''async fn handle_connection(
    mut controller: ServerH3Controller,
    peer: SocketAddr,
    internal: SocketAddr,
    public_port: u16,
    internal_token: HeaderValue,
    client: ProxyClient,
    alt_svc: Option<HeaderValue>,
    _connection_permit: OwnedSemaphorePermit,
) {
''',
    '''#[derive(Clone)]
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
''',
)
replace(
    "src/http3.rs",
    '''                tokio::spawn(proxy_request(
                    incoming_headers,
                    peer,
                    internal,
                    public_port,
                    internal_token.clone(),
                    client.clone(),
                    alt_svc.clone(),
                ));''',
    '''                tokio::spawn(proxy_request(incoming_headers, context.clone()));''',
)
replace(
    "src/http3.rs",
    '''async fn proxy_request(
    incoming: IncomingH3Headers,
    peer: SocketAddr,
    internal: SocketAddr,
    public_port: u16,
    internal_token: HeaderValue,
    client: ProxyClient,
    alt_svc: Option<HeaderValue>,
) {
''',
    '''async fn proxy_request(incoming: IncomingH3Headers, context: Http3ConnectionContext) {
    let Http3ConnectionContext {
        peer,
        internal,
        public_port,
        internal_token,
        client,
        alt_svc,
    } = context;
''',
)
