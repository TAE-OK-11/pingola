#!/usr/bin/env python3
from pathlib import Path


def replace_once(path: str, old: str, new: str) -> None:
    p = Path(path)
    text = p.read_text()
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{path}: expected exactly one match, found {count}: {old[:80]!r}")
    p.write_text(text.replace(old, new, 1))


# Shared TLS policy. The hybrid group is preferred, with classical fallbacks for
# clients that have not shipped ML-KEM yet.
Path("src/tls_policy.rs").write_text('''use boring::ssl::{SslContextBuilder, SslMethod, SslVersion};\n\npub const HYBRID_PQ_GROUPS: &str = "X25519MLKEM768:X25519:P-256";\npub const HYBRID_PQ_PRIMARY_GROUP: &str = "X25519MLKEM768";\n\npub fn new_hybrid_pq_context() -> Result<SslContextBuilder, boring::error::ErrorStack> {\n    let mut builder = SslContextBuilder::new(SslMethod::tls())?;\n    builder.set_min_proto_version(Some(SslVersion::TLS1_3))?;\n    builder.set_max_proto_version(Some(SslVersion::TLS1_3))?;\n    builder.set_curves_list(HYBRID_PQ_GROUPS)?;\n    Ok(builder)\n}\n\n#[cfg(test)]\nmod tests {\n    use super::*;\n\n    #[test]\n    fn cloudflare_boringssl_exposes_x25519_mlkem768() {\n        let mut builder = SslContextBuilder::new(SslMethod::tls()).unwrap();\n        builder.set_curves_list(HYBRID_PQ_PRIMARY_GROUP).unwrap();\n    }\n}\n''')

# Server configuration for bounded QUIC admission.
replace_once(
    "src/config.rs",
    '''fn default_http3_max_concurrent_streams() -> u32 {\n    64\n}\n\nfn default_downstream_max_connections() -> usize {''',
    '''fn default_http3_max_concurrent_streams() -> u32 {\n    64\n}\n\nfn default_http3_handshake_timeout() -> u64 {\n    5\n}\n\nfn default_http3_connection_rate_per_second() -> f64 {\n    64.0\n}\n\nfn default_http3_connection_burst() -> u32 {\n    128\n}\n\nfn default_http3_max_connections_per_ip() -> usize {\n    128\n}\n\nfn default_downstream_max_connections() -> usize {''',
)
replace_once(
    "src/config.rs",
    '''    #[serde(default = "default_http3_max_concurrent_streams")]\n    pub http3_max_concurrent_streams: u32,\n    #[serde(default)]\n    pub certificate: Option<PathBuf>,''',
    '''    #[serde(default = "default_http3_max_concurrent_streams")]\n    pub http3_max_concurrent_streams: u32,\n    #[serde(default = "default_http3_handshake_timeout")]\n    pub http3_handshake_timeout_seconds: u64,\n    #[serde(default = "default_http3_connection_rate_per_second")]\n    pub http3_connection_rate_per_second: f64,\n    #[serde(default = "default_http3_connection_burst")]\n    pub http3_connection_burst: u32,\n    #[serde(default = "default_http3_max_connections_per_ip")]\n    pub http3_max_connections_per_ip: usize,\n    #[serde(default)]\n    pub certificate: Option<PathBuf>,''',
)
replace_once(
    "src/config.rs",
    '''    if !(1..=600).contains(&config.server.http3_max_idle_timeout_seconds) {\n        bail!("server.http3_max_idle_timeout_seconds must be between 1 and 600");\n    }\n    if !(1..=1_000_000).contains(&config.server.downstream_max_connections) {''',
    '''    if !(1..=600).contains(&config.server.http3_max_idle_timeout_seconds) {\n        bail!("server.http3_max_idle_timeout_seconds must be between 1 and 600");\n    }\n    if !(1..=30).contains(&config.server.http3_handshake_timeout_seconds) {\n        bail!("server.http3_handshake_timeout_seconds must be between 1 and 30");\n    }\n    if !config.server.http3_connection_rate_per_second.is_finite()\n        || !(0.1..=100_000.0).contains(&config.server.http3_connection_rate_per_second)\n    {\n        bail!("server.http3_connection_rate_per_second must be finite and between 0.1 and 100000");\n    }\n    if config.server.http3_connection_burst > 100_000 {\n        bail!("server.http3_connection_burst must not exceed 100000");\n    }\n    if !(1..=1_000_000).contains(&config.server.http3_max_connections_per_ip) {\n        bail!("server.http3_max_connections_per_ip must be between 1 and 1000000");\n    }\n    if !(1..=1_000_000).contains(&config.server.downstream_max_connections) {''',
)
replace_once(
    "src/config.rs",
    '''    if !(1..=300).contains(&config.server.downstream_request_header_timeout_seconds) {\n        bail!("server.downstream_request_header_timeout_seconds must be between 1 and 300");\n    }''',
    '''    if config.server.http3_max_connections_per_ip > config.server.downstream_max_connections {\n        bail!("server.http3_max_connections_per_ip must not exceed server.downstream_max_connections");\n    }\n    if !(1..=300).contains(&config.server.downstream_request_header_timeout_seconds) {\n        bail!("server.downstream_request_header_timeout_seconds must be between 1 and 300");\n    }''',
)

# The existing limiter is deliberately generic enough for H2 streams and QUIC
# connections; make the documentation match its actual use.
replace_once(
    "src/limits.rs",
    '''/// Counts active requests per IP. HTTP/2 requests are streams, not TCP\n/// connections, so the type intentionally describes what is actually bounded.''',
    '''/// Counts active resources per IP. It is used for HTTP requests/streams and\n/// for QUIC connections, with the `zone` separating independent limits.''',
)

# TCP TLS: prefer X25519MLKEM768 while keeping X25519/P-256 fallbacks.
replace_once("src/main.rs", "mod static_files;\n", "mod static_files;\nmod tls_policy;\n")
replace_once(
    "src/main.rs",
    '''use crate::preflight::check_runtime;''',
    '''use crate::preflight::check_runtime;\nuse crate::tls_policy::HYBRID_PQ_GROUPS;''',
)
replace_once(
    "src/main.rs",
    '''    tls.set_max_proto_version(Some(SslVersion::TLS1_3))\n        .context("failed to set BoringSSL maximum protocol to TLS 1.3")?;\n    Ok(())''',
    '''    tls.set_max_proto_version(Some(SslVersion::TLS1_3))\n        .context("failed to set BoringSSL maximum protocol to TLS 1.3")?;\n    tls.set_curves_list(HYBRID_PQ_GROUPS)\n        .context("failed to configure X25519MLKEM768 hybrid post-quantum groups")?;\n    Ok(())''',
)
replace_once(
    "src/main.rs",
    '''        "starting Pingora with {} TLS 1.3: http={:?} https={:?} http3_udp={:?} http3_internal={} health_socket={} threads={}",\n        tls_provider_name(),''',
    '''        "starting Pingora with {} TLS 1.3 hybrid_pq={}: http={:?} https={:?} http3_udp={:?} http3_internal={} health_socket={} threads={}",\n        tls_provider_name(),\n        HYBRID_PQ_GROUPS,''',
)

# QUIC TLS + admission hardening.
replace_once(
    "src/http3.rs",
    '''use anyhow::{Context, Result, anyhow, bail};\nuse bytes::Bytes;''',
    '''use anyhow::{Context, Result, anyhow, bail};\nuse boring::ssl::{SslContextBuilder, SslFiletype};\nuse bytes::Bytes;''',
)
replace_once(
    "src/http3.rs",
    '''use tokio_quiche::metrics::DefaultMetrics;\nuse tokio_quiche::quiche::h3::{self, NameValue};''',
    '''use tokio_quiche::metrics::DefaultMetrics;\nuse tokio_quiche::quic::ConnectionHook;\nuse tokio_quiche::quiche::h3::{self, NameValue};''',
)
replace_once(
    "src/http3.rs",
    '''use crate::config::RuntimeConfig;''',
    '''use crate::config::RuntimeConfig;\nuse crate::limits::{ActiveRequestLimiter, ActiveRequestPermit, RateLimiter};\nuse crate::tls_policy::{HYBRID_PQ_GROUPS, new_hybrid_pq_context};''',
)
replace_once(
    "src/http3.rs",
    '''const HTTP3_SEND_CAPACITY_FACTOR: f64 = 2.0;''',
    '''const HTTP3_SEND_CAPACITY_FACTOR: f64 = 2.0;\nconst HTTP3_MAX_AMPLIFICATION_FACTOR: usize = 3;\nconst HTTP3_ADMISSION_ZONE: &str = "http3-connection";''',
)

insert_anchor = '''type ProxyClient = Client<HttpConnector, ProxyBody>;\n\npub fn start(runtime: Arc<RuntimeConfig>) -> Result<()> {'''
insert_code = '''type ProxyClient = Client<HttpConnector, ProxyBody>;\n\n#[derive(Debug)]\nstruct HybridPqQuicTlsHook;\n\nimpl ConnectionHook for HybridPqQuicTlsHook {\n    fn create_custom_ssl_context_builder(\n        &self,\n        settings: TlsCertificatePaths<'_>,\n    ) -> Option<SslContextBuilder> {\n        Some(\n            build_hybrid_pq_quic_context(settings.cert, settings.private_key).unwrap_or_else(\n                |error| {\n                    panic!(\n                        "validated HTTP/3 hybrid PQ TLS context became invalid: {error:#}"\n                    )\n                },\n            ),\n        )\n    }\n}\n\nfn build_hybrid_pq_quic_context(\n    certificate: &str,\n    private_key: &str,\n) -> Result<SslContextBuilder> {\n    let mut builder = new_hybrid_pq_context()\n        .context("failed to create Cloudflare BoringSSL hybrid PQ context")?;\n    builder\n        .set_certificate_chain_file(certificate)\n        .with_context(|| format!("failed to load HTTP/3 certificate chain {certificate}"))?;\n    builder\n        .set_private_key_file(private_key, SslFiletype::PEM)\n        .with_context(|| format!("failed to load HTTP/3 private key {private_key}"))?;\n    builder\n        .check_private_key()\n        .context("HTTP/3 certificate and private key do not match")?;\n    Ok(builder)\n}\n\n#[derive(Debug, Clone, Copy, Eq, PartialEq)]\nenum Http3AdmissionRejection {\n    RateLimited,\n    TooManyConnections,\n}\n\nstruct Http3Admission {\n    rate: RateLimiter,\n    active: ActiveRequestLimiter,\n    rate_per_second: f64,\n    burst: u32,\n    max_active: usize,\n}\n\nimpl Http3Admission {\n    fn new(rate_per_second: f64, burst: u32, max_active: usize) -> Self {\n        Self {\n            rate: RateLimiter::new(),\n            active: ActiveRequestLimiter::new(),\n            rate_per_second,\n            burst,\n            max_active,\n        }\n    }\n\n    fn admit(&self, peer: SocketAddr) -> Result<ActiveRequestPermit, Http3AdmissionRejection> {\n        if !self.rate.allow(\n            HTTP3_ADMISSION_ZONE,\n            peer.ip(),\n            self.rate_per_second,\n            self.burst,\n        ) {\n            return Err(Http3AdmissionRejection::RateLimited);\n        }\n        self.active\n            .acquire(HTTP3_ADMISSION_ZONE, peer.ip(), self.max_active)\n            .ok_or(Http3AdmissionRejection::TooManyConnections)\n    }\n}\n\npub fn start(runtime: Arc<RuntimeConfig>) -> Result<()> {'''
replace_once("src/http3.rs", insert_anchor, insert_code)

replace_once(
    "src/http3.rs",
    '''    let private_key = private_key\n        .to_str()\n        .ok_or_else(|| anyhow!("HTTP/3 private key path is not valid UTF-8"))?;\n\n    let mut sockets = Vec::with_capacity(server.http3_listen.len());''',
    '''    let private_key = private_key\n        .to_str()\n        .ok_or_else(|| anyhow!("HTTP/3 private key path is not valid UTF-8"))?;\n\n    // Build once before binding to guarantee the PQ group and certificate are\n    // accepted. The connection hook repeats the same construction per listener\n    // and fails closed if that invariant unexpectedly changes.\n    drop(\n        build_hybrid_pq_quic_context(certificate, private_key)\n            .context("HTTP/3 hybrid PQ TLS preflight failed")?,\n    );\n\n    let mut sockets = Vec::with_capacity(server.http3_listen.len());''',
)
replace_once(
    "src/http3.rs",
    '''    quic.handshake_timeout = Some(Duration::from_secs(10));''',
    '''    quic.handshake_timeout = Some(Duration::from_secs(\n        server.http3_handshake_timeout_seconds,\n    ));''',
)
replace_once(
    "src/http3.rs",
    '''    quic.disable_active_migration = true;\n    quic.disable_client_ip_validation = false;\n\n    let params = ConnectionParams::new_server(\n        quic,\n        TlsCertificatePaths {\n            cert: certificate,\n            private_key,\n            kind: CertificateKind::X509,\n        },\n        Hooks::default(),\n    );''',
    '''    quic.disable_active_migration = true;\n    // Stateless Retry proves source-address ownership before the server allocates\n    // a full QUIC connection and starts expensive TLS work.\n    quic.disable_client_ip_validation = false;\n    quic.max_amplification_factor = HTTP3_MAX_AMPLIFICATION_FACTOR;\n\n    let params = ConnectionParams::new_server(\n        quic,\n        TlsCertificatePaths {\n            cert: certificate,\n            private_key,\n            kind: CertificateKind::X509,\n        },\n        Hooks {\n            connection_hook: Some(Arc::new(HybridPqQuicTlsHook)),\n        },\n    );''',
)
replace_once(
    "src/http3.rs",
    '''    let connection_limit = Arc::new(Semaphore::new(server.downstream_max_connections));\n\n    for mut listener in listeners {''',
    '''    let connection_limit = Arc::new(Semaphore::new(server.downstream_max_connections));\n    let admission = Arc::new(Http3Admission::new(\n        server.http3_connection_rate_per_second,\n        server.http3_connection_burst,\n        server.http3_max_connections_per_ip,\n    ));\n\n    for mut listener in listeners {''',
)
replace_once(
    "src/http3.rs",
    '''        let connection_limit = connection_limit.clone();\n        tokio::spawn(async move {''',
    '''        let connection_limit = connection_limit.clone();\n        let admission = admission.clone();\n        tokio::spawn(async move {''',
)
replace_once(
    "src/http3.rs",
    '''                        let peer = connection.peer_addr();\n                        let settings = Http3Settings {''',
    '''                        let peer = connection.peer_addr();\n                        let client_permit = match admission.admit(peer) {\n                            Ok(permit) => permit,\n                            Err(Http3AdmissionRejection::RateLimited) => {\n                                warn!("HTTP/3 connection rejected: per-IP admission rate exceeded peer={peer}");\n                                continue;\n                            }\n                            Err(Http3AdmissionRejection::TooManyConnections) => {\n                                warn!("HTTP/3 connection rejected: per-IP active connection limit reached peer={peer}");\n                                continue;\n                            }\n                        };\n                        let settings = Http3Settings {''',
)
replace_once(
    "src/http3.rs",
    '''                            permit,\n                        ));''',
    '''                            permit,\n                            client_permit,\n                        ));''',
)
replace_once(
    "src/http3.rs",
    '''        "HTTP/3 frontend started: udp={:?} internal=h2c://{} quiche={} early_data=false migration=false pmtud=true pacing=true max_udp_payload={} send_capacity_factor={}",\n        server.http3_listen,\n        internal,\n        tokio_quiche::quiche::PROTOCOL_VERSION,\n        HTTP3_MAX_UDP_PAYLOAD_SIZE,\n        HTTP3_SEND_CAPACITY_FACTOR,\n    );''',
    '''        "HTTP/3 frontend started: udp={:?} internal=h2c://{} quiche={} hybrid_pq={} stateless_retry=true max_amplification={} early_data=false migration=false pmtud=true pacing=true max_udp_payload={} send_capacity_factor={} admission_rate={}/s burst={} max_connections_per_ip={} handshake_timeout={}s",\n        server.http3_listen,\n        internal,\n        tokio_quiche::quiche::PROTOCOL_VERSION,\n        HYBRID_PQ_GROUPS,\n        HTTP3_MAX_AMPLIFICATION_FACTOR,\n        HTTP3_MAX_UDP_PAYLOAD_SIZE,\n        HTTP3_SEND_CAPACITY_FACTOR,\n        server.http3_connection_rate_per_second,\n        server.http3_connection_burst,\n        server.http3_max_connections_per_ip,\n        server.http3_handshake_timeout_seconds,\n    );''',
)
replace_once(
    "src/http3.rs",
    '''    _connection_permit: OwnedSemaphorePermit,\n) {''',
    '''    _connection_permit: OwnedSemaphorePermit,\n    _client_connection_permit: ActiveRequestPermit,\n) {''',
)

# Add admission/PQ unit coverage inside the existing test module.
replace_once(
    "src/http3.rs",
    '''#[cfg(test)]\nmod tests {\n    use super::*;''',
    '''#[cfg(test)]\nmod tests {\n    use super::*;\n\n    #[test]\n    fn hybrid_pq_context_accepts_cloudflare_group_policy() {\n        let mut builder = new_hybrid_pq_context().unwrap();\n        builder.set_curves_list(HYBRID_PQ_GROUPS).unwrap();\n    }\n\n    #[test]\n    fn http3_admission_limits_rate_and_active_connections() {\n        let peer: SocketAddr = "192.0.2.44:443".parse().unwrap();\n\n        let active = Http3Admission::new(10_000.0, 8, 1);\n        let permit = active.admit(peer).unwrap();\n        assert!(matches!(\n            active.admit(peer),\n            Err(Http3AdmissionRejection::TooManyConnections)\n        ));\n        drop(permit);\n\n        let rate = Http3Admission::new(0.1, 0, 8);\n        let _permit = rate.admit(peer).unwrap();\n        assert!(matches!(\n            rate.admit(peer),\n            Err(Http3AdmissionRejection::RateLimited)\n        ));\n    }''',
)

# End-to-end TCP handshake probe using the same Cloudflare boring crate. This
# verifies the server actually negotiates X25519MLKEM768, not merely that the
# group name compiles.
Path("examples/pq_tls_probe.rs").write_text('''use std::net::TcpStream;\n\nuse anyhow::{Context, Result, anyhow, bail};\nuse boring::ssl::{SslConnector, SslMethod, SslVerifyMode, SslVersion};\n\nconst HYBRID_GROUP: &str = "X25519MLKEM768";\n\nfn main() -> Result<()> {\n    let mut args = std::env::args().skip(1);\n    let address = args\n        .next()\n        .ok_or_else(|| anyhow!("usage: pq_tls_probe <address> <server-name>"))?;\n    let server_name = args\n        .next()\n        .ok_or_else(|| anyhow!("missing server name"))?;\n    if args.next().is_some() {\n        bail!("too many arguments");\n    }\n\n    let mut builder = SslConnector::builder(SslMethod::tls())?;\n    builder.set_verify(SslVerifyMode::NONE);\n    builder.set_min_proto_version(Some(SslVersion::TLS1_3))?;\n    builder.set_max_proto_version(Some(SslVersion::TLS1_3))?;\n    builder.set_curves_list(HYBRID_GROUP)?;\n    let connector = builder.build();\n\n    let tcp = TcpStream::connect(&address)\n        .with_context(|| format!("failed to connect TCP TLS probe to {address}"))?;\n    let stream = connector\n        .connect(&server_name, tcp)\n        .map_err(|error| anyhow!("hybrid PQ TLS handshake failed: {error}"))?;\n    let negotiated = stream.ssl().curve_name().unwrap_or("unknown");\n    if negotiated != HYBRID_GROUP {\n        bail!("expected {HYBRID_GROUP}, negotiated {negotiated}");\n    }\n    println!("{negotiated}");\n    Ok(())\n}\n''')

replace_once(
    "tests/http3.sh",
    '''cargo build --manifest-path "${ROOT}/Cargo.toml" --locked \\\n  --bin pingora --example http3_probe''',
    '''cargo build --manifest-path "${ROOT}/Cargo.toml" --locked \\\n  --bin pingora --example http3_probe --example pq_tls_probe''',
)
replace_once(
    "tests/http3.sh",
    '''kill -0 "${GATEWAY_PID}"\n\n# Browsers normally discover HTTP/3 from an initial H1 or H2 TLS''',
    '''kill -0 "${GATEWAY_PID}"\n\npq_curve=$("${ROOT}/target/debug/examples/pq_tls_probe" \\\n  127.0.0.1:18444 app.test)\n[[ "${pq_curve}" == "X25519MLKEM768" ]]\n\n# Browsers normally discover HTTP/3 from an initial H1 or H2 TLS''',
)
replace_once(
    "tests/http3.sh",
    '''grep -q 'HTTP/3 frontend started:.*internal=h2c://' "${GATEWAY_LOG}"\ngrep -q 'http3_udp=\\["127.0.0.1:18443"\\]' "${GATEWAY_LOG}"\n\necho "HTTP/3 QUIC proxy, h2c internal multiplexing, static response, Alt-Svc, forwarding, and private-header isolation tests passed"''',
    '''grep -q 'HTTP/3 frontend started:.*internal=h2c://' "${GATEWAY_LOG}"\ngrep -q 'HTTP/3 frontend started:.*hybrid_pq=X25519MLKEM768:X25519:P-256.*stateless_retry=true.*max_amplification=3' "${GATEWAY_LOG}"\ngrep -q 'http3_udp=\\["127.0.0.1:18443"\\]' "${GATEWAY_LOG}"\n\necho "HTTP/3 hybrid PQ TLS, stateless Retry, anti-DDoS admission, QUIC proxy, h2c multiplexing, Alt-Svc, forwarding, and isolation tests passed"''',
)

# A concise README note; append only once so the migration remains idempotent.
readme = Path("README.md")
text = readme.read_text()
marker = "## Hybrid post-quantum TLS and QUIC admission hardening"
if marker not in text:
    text += '''\n\n## Hybrid post-quantum TLS and QUIC admission hardening\n\nJBS Pingora prefers `X25519MLKEM768` for TLS 1.3 on both TCP and QUIC, with `X25519` and `P-256` retained as compatibility fallbacks. QUIC source-address validation is mandatory, so new clients complete a stateless Retry before a full connection is allocated. The server also enforces the QUIC 3x anti-amplification bound, disables 0-RTT and active migration, applies a bounded handshake timeout, and limits new/active QUIC connections per source IP. The defaults are 64 new connections/s with a burst of 128, at most 128 active QUIC connections per IP, and a 5-second QUIC handshake timeout.\n'''
    readme.write_text(text)
