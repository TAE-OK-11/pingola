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
    "Cargo.toml",
    'futures = "0.3.33"\n',
    'futures = "0.3.33"\ngetrandom = "0.4.3"\nhex = "0.4.3"\n',
)
replace(
    "src/config.rs",
    "use std::fs;\n",
    "use std::fmt;\nuse std::fs;\n",
)
replace(
    "src/config.rs",
    "use anyhow::{Context, Result, bail};",
    "use anyhow::{Context, Result, anyhow, bail};",
)
replace(
    "src/config.rs",
    '''#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    pub config: Arc<Config>,
}
''',
    '''#[derive(Clone)]
struct Http3InternalToken(HeaderValue);

impl fmt::Debug for Http3InternalToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Http3InternalToken([redacted])")
    }
}

#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    pub config: Arc<Config>,
    http3_internal_token: Option<Http3InternalToken>,
}
''',
)
replace(
    "src/config.rs",
    '''    pub fn new(config: Config) -> Result<Self> {
        validate(&config)?;

        Ok(Self {
            config: Arc::new(config),
        })
    }
''',
    '''    pub fn new(config: Config) -> Result<Self> {
        validate(&config)?;
        let http3_internal_token = if config.server.http3_listen.is_empty() {
            None
        } else {
            let mut token = [0_u8; 32];
            getrandom::fill(&mut token)
                .map_err(|error| anyhow!("failed to generate HTTP/3 internal token: {error}"))?;
            let token = HeaderValue::from_str(&hex::encode(token))
                .context("generated HTTP/3 internal token is not a valid header value")?;
            Some(Http3InternalToken(token))
        };

        Ok(Self {
            config: Arc::new(config),
            http3_internal_token,
        })
    }
''',
)
replace(
    "src/config.rs",
    '''    pub fn http3_internal_addr(&self) -> Option<SocketAddr> {
''',
    '''    pub fn http3_internal_token(&self) -> Option<&HeaderValue> {
        self.http3_internal_token.as_ref().map(|token| &token.0)
    }

    pub fn http3_internal_addr(&self) -> Option<SocketAddr> {
''',
)

replace(
    "src/http3.rs",
    '''    let public_port = runtime
        .http3_public_port()
        .ok_or_else(|| anyhow!("HTTP/3 public port was not configured"))?;
    let alt_svc = runtime.http3_alt_svc_header();
''',
    '''    let public_port = runtime
        .http3_public_port()
        .ok_or_else(|| anyhow!("HTTP/3 public port was not configured"))?;
    let internal_token = runtime
        .http3_internal_token()
        .cloned()
        .ok_or_else(|| anyhow!("HTTP/3 internal token was not initialized"))?;
    let alt_svc = runtime.http3_alt_svc_header();
''',
)
replace(
    "src/http3.rs",
    '''        let alt_svc = alt_svc.clone();
        let connection_limit = connection_limit.clone();
''',
    '''        let alt_svc = alt_svc.clone();
        let internal_token = internal_token.clone();
        let connection_limit = connection_limit.clone();
''',
)
replace(
    "src/http3.rs",
    '''                            public_port,
                            client.clone(),
''',
    '''                            public_port,
                            internal_token.clone(),
                            client.clone(),
''',
)
replace(
    "src/http3.rs",
    '''    public_port: u16,
    client: ProxyClient,
    alt_svc: Option<HeaderValue>,
    _connection_permit: OwnedSemaphorePermit,
''',
    '''    public_port: u16,
    internal_token: HeaderValue,
    client: ProxyClient,
    alt_svc: Option<HeaderValue>,
    _connection_permit: OwnedSemaphorePermit,
''',
)
replace(
    "src/http3.rs",
    '''                    public_port,
                    client.clone(),
''',
    '''                    public_port,
                    internal_token.clone(),
                    client.clone(),
''',
)
replace(
    "src/http3.rs",
    '''    public_port: u16,
    client: ProxyClient,
    alt_svc: Option<HeaderValue>,
) {
''',
    '''    public_port: u16,
    internal_token: HeaderValue,
    client: ProxyClient,
    alt_svc: Option<HeaderValue>,
) {
''',
)
replace(
    "src/http3.rs",
    '''        public_port,
        client,
''',
    '''        public_port,
        internal_token,
        client,
''',
)
replace(
    "src/http3.rs",
    '''    public_port: u16,
    client: ProxyClient,
    alt_svc: Option<HeaderValue>,
) -> Result<()> {
    let decoded = match decode_request_headers(&headers, peer, internal, public_port) {
''',
    '''    public_port: u16,
    internal_token: HeaderValue,
    client: ProxyClient,
    alt_svc: Option<HeaderValue>,
) -> Result<()> {
    let decoded = match decode_request_headers(
        &headers,
        peer,
        internal,
        public_port,
        internal_token,
    ) {
''',
)
replace(
    "src/http3.rs",
    '''    internal: SocketAddr,
    public_port: u16,
) -> Result<DecodedRequest> {
''',
    '''    internal: SocketAddr,
    public_port: u16,
    internal_token: HeaderValue,
) -> Result<DecodedRequest> {
''',
)
replace(
    "src/http3.rs",
    '''    output.insert("x-forwarded-port", public_port.clone());
    output.insert(INTERNAL_MARKER, HeaderValue::from_static("1"));
    output.insert(INTERNAL_PORT, public_port);
''',
    '''    output.insert("x-forwarded-port", public_port.clone());
    output.insert(INTERNAL_MARKER, internal_token);
    output.insert(INTERNAL_PORT, public_port);
''',
)
replace(
    "src/http3.rs",
    '''                443,
            )
''',
    '''                443,
                HeaderValue::from_static("unit-test-token"),
            )
''',
)
replace(
    "src/http3.rs",
    '''            8443,
        )
''',
    '''            8443,
            HeaderValue::from_static("unit-test-token"),
        )
''',
)
replace(
    "src/http3.rs",
    '        assert_eq!(request.headers[INTERNAL_MARKER], "1");\n',
    '        assert_eq!(request.headers[INTERNAL_MARKER], "unit-test-token");\n',
)

replace(
    "src/gateway.rs",
    '''    let marker_matches = session
        .req_header()
        .headers
        .get(&HTTP3_INTERNAL)
        .is_some_and(|value| value == "1");
''',
    '''    let marker_matches = runtime.http3_internal_token().is_some_and(|expected| {
        session
            .req_header()
            .headers
            .get(&HTTP3_INTERNAL)
            .is_some_and(|value| value == expected)
    });
''',
)

replace(
    "tests/http3.sh",
    '''[[ "${location}" == "https://app.test/headers" ]]

grep -q 'HTTP/3 frontend started' "${GATEWAY_LOG}"
''',
    '''[[ "${location}" == "https://app.test/headers" ]]

# A loopback caller cannot forge the private H3 handoff with the old static
# marker. The request must still be treated as plaintext and redirected.
spoof_location=$(curl --noproxy '*' -sSI -H 'host: app.test' \
  -H 'x-jbs-http3-internal: 1' -H 'x-jbs-http3-port: 18443' \
  http://127.0.0.1:18080/headers | awk -F': ' \
  'tolower($1) == "location" {gsub("\\r", "", $2); print $2}')
[[ "${spoof_location}" == "https://app.test/headers" ]]

grep -q 'HTTP/3 frontend started' "${GATEWAY_LOG}"
''',
)
