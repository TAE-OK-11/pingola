from pathlib import Path


def replace(path, old, new, count=1):
    p = Path(path)
    text = p.read_text()
    if old not in text:
        raise SystemExit(f"missing replacement in {path}: {old[:140]!r}")
    p.write_text(text.replace(old, new, count))

# Secure default stays ON. Trusted private H3 origins can explicitly disable
# Retry so accepted 0-RTT data is not forced through address-validation Retry.
replace(
    "src/config.rs",
    '''    #[serde(default)]
    pub http3_enable_early_data: bool,
''',
    '''    #[serde(default)]
    pub http3_enable_early_data: bool,
    #[serde(default = "default_true")]
    pub http3_stateless_retry: bool,
''',
)

replace(
    "src/http3.rs",
    '''    let allow_early_data = server.http3_enable_early_data;
''',
    '''    let allow_early_data = server.http3_enable_early_data;
    let stateless_retry = server.http3_stateless_retry;
''',
)
replace(
    "src/http3.rs",
    '''    // Stateless Retry proves source-address ownership before the server allocates
    // a full QUIC connection and starts expensive TLS work.
    quic.disable_client_ip_validation = false;
''',
    '''    // Stateless Retry proves source-address ownership before the server allocates
    // a full QUIC connection and starts expensive TLS work. Keep it enabled by
    // default for public listeners; trusted private origins may explicitly turn
    // it off to permit true accepted 0-RTT without a Retry round trip.
    quic.disable_client_ip_validation = !stateless_retry;
''',
)
replace(
    "src/http3.rs",
    '''        "HTTP/3 frontend started: udp={:?} internal=h2c://{} quiche={} hybrid_pq={} stateless_retry=true max_amplification={} early_data={} migration=false pmtud=true pacing=true socket_offload=auto[gso,gro,so_txtime,rxq_ovfl,pmtu_probe] max_udp_payload={} send_capacity_factor={} admission_rate={}/s burst={} max_connections_per_ip={} handshake_timeout={}s",
''',
    '''        "HTTP/3 frontend started: udp={:?} internal=h2c://{} quiche={} hybrid_pq={} stateless_retry={} max_amplification={} early_data={} migration=false pmtud=true pacing=true socket_offload=auto[gso,gro,so_txtime,rxq_ovfl,pmtu_probe] max_udp_payload={} send_capacity_factor={} admission_rate={}/s burst={} max_connections_per_ip={} handshake_timeout={}s",
''',
)
replace(
    "src/http3.rs",
    '''        HYBRID_PQ_GROUPS,
        HTTP3_MAX_AMPLIFICATION_FACTOR,
        allow_early_data,
''',
    '''        HYBRID_PQ_GROUPS,
        stateless_retry,
        HTTP3_MAX_AMPLIFICATION_FACTOR,
        allow_early_data,
''',
)

replace(
    "config/pingora.yaml",
    '''  http3_enable_early_data: false
''',
    '''  http3_enable_early_data: false
  http3_stateless_retry: true
''',
)
replace(
    "tests/fixtures/upstream_http3_origin.yaml",
    '''  http3_enable_early_data: true
''',
    '''  http3_enable_early_data: true
  # This fixture models a trusted private origin. Public listeners keep the
  # default Retry protection enabled; private origin 0-RTT explicitly opts out.
  http3_stateless_retry: false
''',
)

# Keep test documentation explicit about the security boundary.
replace(
    "README.md",
    '''- upstream HTTP/3/QUIC (`http3`/`http3-preferred`) + connection reuse + replay-safe 0-RTT session resumption
''',
    '''- upstream HTTP/3/QUIC (`http3`/`http3-preferred`) + connection reuse + replay-safe 0-RTT session resumption
- QUIC Stateless Retry defaults ON; only a trusted private H3 origin should set `server.http3_stateless_retry: false` when accepted 0-RTT is required
''',
)
