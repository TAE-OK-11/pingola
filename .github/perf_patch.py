from pathlib import Path
import re


def read(path: str) -> str:
    return Path(path).read_text()


def write(path: str, text: str) -> None:
    Path(path).write_text(text)


def replace_once(path: str, old: str, new: str) -> None:
    text = read(path)
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{path}: expected one match, found {count}: {old[:100]!r}")
    write(path, text.replace(old, new, 1))


def regex_once(path: str, pattern: str, repl: str) -> None:
    text = read(path)
    updated, count = re.subn(pattern, repl, text, count=1, flags=re.MULTILINE)
    if count != 1:
        raise SystemExit(f"{path}: regex expected one match, found {count}: {pattern[:100]!r}")
    write(path, updated)


# Server/upstream defaults: larger pool and more multiplexing headroom.
regex_once(
    "src/config.rs",
    r"fn default_keepalive_pool\(\) -> usize \{\n\s+128\n\}",
    "fn default_keepalive_pool() -> usize {\n    512\n}",
)
regex_once(
    "src/config.rs",
    r"fn default_upstream_http2_streams\(\) -> usize \{\n\s+32\n\}\n\nfn default_upstream_http3_streams\(\) -> usize \{\n\s+64\n\}",
    """fn default_upstream_http2_streams() -> usize {
    128
}

fn default_upstream_http2_stream_window_bytes() -> u32 {
    16 * 1024 * 1024
}

fn default_upstream_http2_connection_window_bytes() -> u32 {
    64 * 1024 * 1024
}

fn default_upstream_http2_ping_interval_seconds() -> u64 {
    30
}

fn default_upstream_http3_streams() -> usize {
    128
}""",
)
replace_once(
    "src/config.rs",
    '    #[serde(default = "default_upstream_http2_streams")]\n    pub http2_max_concurrent_streams: usize,\n',
    '    #[serde(default = "default_upstream_http2_streams")]\n'
    '    pub http2_max_concurrent_streams: usize,\n'
    '    #[serde(default = "default_upstream_http2_stream_window_bytes")]\n'
    '    pub http2_stream_window_bytes: u32,\n'
    '    #[serde(default = "default_upstream_http2_connection_window_bytes")]\n'
    '    pub http2_connection_window_bytes: u32,\n'
    '    #[serde(default = "default_upstream_http2_ping_interval_seconds")]\n'
    '    pub http2_ping_interval_seconds: u64,\n',
)
validation_anchor = '''        if !(1..=1024).contains(&upstream.http2_max_concurrent_streams) {
            bail!("upstream {name} http2_max_concurrent_streams must be between 1 and 1024");
        }
'''
replace_once(
    "src/config.rs",
    validation_anchor,
    validation_anchor
    + '''        const H2_MAX_WINDOW_SIZE: u32 = (1_u32 << 31) - 1;
        if !(1..=H2_MAX_WINDOW_SIZE).contains(&upstream.http2_stream_window_bytes) {
            bail!("upstream {name} http2_stream_window_bytes must be between 1 and 2^31-1");
        }
        if !(1..=H2_MAX_WINDOW_SIZE).contains(&upstream.http2_connection_window_bytes) {
            bail!("upstream {name} http2_connection_window_bytes must be between 1 and 2^31-1");
        }
        if upstream.http2_connection_window_bytes < upstream.http2_stream_window_bytes {
            bail!("upstream {name} http2_connection_window_bytes must be at least http2_stream_window_bytes");
        }
        if upstream.http2_ping_interval_seconds > 3600 {
            bail!("upstream {name} http2_ping_interval_seconds must not exceed 3600");
        }
''',
)
replace_once(
    "src/config.rs",
    "        assert_eq!(automatic.http2_max_concurrent_streams, 32);\n",
    "        assert_eq!(automatic.http2_max_concurrent_streams, 128);\n"
    "        assert_eq!(automatic.http2_stream_window_bytes, 16 * 1024 * 1024);\n"
    "        assert_eq!(automatic.http2_connection_window_bytes, 64 * 1024 * 1024);\n"
    "        assert_eq!(automatic.http2_ping_interval_seconds, 30);\n",
)

# Backport Cloudflare main H2 window controls into the pinned 0.8.1 vendor.
replace_once(
    "vendor/pingora-core-0.8.1/src/upstreams/peer.rs",
    "    // how many concurrent h2 stream are allowed in the same connection\n"
    "    pub max_h2_streams: usize,\n",
    "    // how many concurrent h2 stream are allowed in the same connection\n"
    "    pub max_h2_streams: usize,\n"
    "    /// Initial per-stream H2 receive window size in bytes.\n"
    "    /// If `None`, the default of 8MB is used.\n"
    "    pub h2_stream_window_size: Option<u32>,\n"
    "    /// Initial connection-level H2 receive window size in bytes.\n"
    "    /// If `None`, the default of 8MB is used.\n"
    "    pub h2_connection_window_size: Option<u32>,\n",
)
replace_once(
    "vendor/pingora-core-0.8.1/src/upstreams/peer.rs",
    "            max_h2_streams: 1,\n",
    "            max_h2_streams: 1,\n"
    "            h2_stream_window_size: None,\n"
    "            h2_connection_window_size: None,\n",
)

v2 = "vendor/pingora-core-0.8.1/src/connectors/http/v2.rs"
replace_once(
    v2,
    "        let max_h2_stream = peer.get_peer_options().map_or(1, |o| o.max_h2_streams);\n"
    "        let conn = handshake(stream, max_h2_stream, peer.h2_ping_interval()).await?;\n",
    "        let peer_options = peer.get_peer_options();\n"
    "        let mut settings = H2HandshakeSettings::new();\n"
    "        settings.max_streams = peer_options.map_or(1, |o| o.max_h2_streams);\n"
    "        settings.ping_interval = peer.h2_ping_interval();\n"
    "        settings.stream_window_size = peer_options.and_then(|o| o.h2_stream_window_size);\n"
    "        settings.connection_window_size =\n"
    "            peer_options.and_then(|o| o.h2_connection_window_size);\n"
    "        let conn = handshake(stream, settings).await?;\n",
)
replace_once(
    v2,
    "            #[cfg(unix)]\n"
    "            if !peer.matches_fd(conn.id()) {\n"
    "                return Ok(None);\n"
    "            }\n",
    "            #[cfg(unix)]\n"
    "            {\n"
    "                let cached_peer_matches = conn\n"
    "                    .digest()\n"
    "                    .socket_digest\n"
    "                    .as_ref()\n"
    "                    .is_some_and(|digest| peer.matches_cached_peer_addr(digest.peer_addr()));\n"
    "                if !cached_peer_matches && !peer.matches_fd(conn.id()) {\n"
    "                    return Ok(None);\n"
    "                }\n"
    "            }\n",
)
replace_once(
    v2,
    "const H2_WINDOW_SIZE: u32 = 1 << 23;\n",
    "const H2_WINDOW_SIZE: u32 = 1 << 23;\n"
    "const H2_MAX_WINDOW_SIZE: u32 = (1_u32 << 31) - 1;\n\n"
    "#[derive(Debug, Clone, Default)]\n"
    "#[non_exhaustive]\n"
    "pub struct H2HandshakeSettings {\n"
    "    pub max_streams: usize,\n"
    "    pub ping_interval: Option<Duration>,\n"
    "    pub stream_window_size: Option<u32>,\n"
    "    pub connection_window_size: Option<u32>,\n"
    "}\n\n"
    "impl H2HandshakeSettings {\n"
    "    pub fn new() -> Self {\n"
    "        Self::default()\n"
    "    }\n"
    "}\n",
)
replace_once(
    v2,
    "pub async fn handshake(\n"
    "    stream: Stream,\n"
    "    max_streams: usize,\n"
    "    h2_ping_interval: Option<Duration>,\n"
    ") -> Result<ConnectionRef> {\n",
    "pub async fn handshake(stream: Stream, settings: H2HandshakeSettings) -> Result<ConnectionRef> {\n",
)
replace_once(
    v2,
    "    use pingora_runtime::current_handle;\n\n"
    "    // Safe guard: new_http_session() assumes there should be at least one free stream\n",
    "    use pingora_runtime::current_handle;\n\n"
    "    let max_streams = settings.max_streams;\n"
    "    if settings\n"
    "        .stream_window_size\n"
    "        .is_some_and(|window| window == 0 || window > H2_MAX_WINDOW_SIZE)\n"
    "    {\n"
    "        return Error::e_explain(H2Error, format!(\"stream_window_size must be between 1 and {H2_MAX_WINDOW_SIZE}\"));\n"
    "    }\n"
    "    if settings\n"
    "        .connection_window_size\n"
    "        .is_some_and(|window| window == 0 || window > H2_MAX_WINDOW_SIZE)\n"
    "    {\n"
    "        return Error::e_explain(H2Error, format!(\"connection_window_size must be between 1 and {H2_MAX_WINDOW_SIZE}\"));\n"
    "    }\n\n"
    "    // Safe guard: new_http_session() assumes there should be at least one free stream\n",
)
replace_once(
    v2,
    "    // TODO: make these configurable\n"
    "    let (send_req, connection) = Builder::new()\n",
    "    let stream_window = settings.stream_window_size.unwrap_or(H2_WINDOW_SIZE);\n"
    "    let connection_window = settings.connection_window_size.unwrap_or(H2_WINDOW_SIZE);\n"
    "    let (send_req, connection) = Builder::new()\n",
)
replace_once(
    v2,
    "        .initial_window_size(H2_WINDOW_SIZE)\n"
    "        // should this be max_streams * H2_WINDOW_SIZE?\n"
    "        .initial_connection_window_size(H2_WINDOW_SIZE)\n",
    "        .initial_window_size(stream_window)\n"
    "        .initial_connection_window_size(connection_window)\n",
)
replace_once(v2, "            h2_ping_interval,\n", "            settings.ping_interval,\n")

# Apply the H2 settings once to precomputed peers; hot requests only reuse them.
replace_once(
    "src/gateway.rs",
    "    peer.options.max_h2_streams = upstream.http2_max_concurrent_streams;\n",
    "    peer.options.max_h2_streams = upstream.http2_max_concurrent_streams;\n"
    "    peer.options.h2_stream_window_size = Some(upstream.http2_stream_window_bytes);\n"
    "    peer.options.h2_connection_window_size = Some(upstream.http2_connection_window_bytes);\n"
    "    peer.options.h2_ping_interval = (upstream.http2_ping_interval_seconds > 0)\n"
    "        .then_some(Duration::from_secs(upstream.http2_ping_interval_seconds));\n",
)
replace_once(
    "src/gateway.rs",
    "        peer.options.max_h2_streams = upstream.http3_max_concurrent_streams;\n",
    "        peer.options.max_h2_streams = upstream.http3_max_concurrent_streams;\n"
    "        peer.options.h2_stream_window_size = Some(2 * 1024 * 1024);\n"
    "        peer.options.h2_connection_window_size = Some(32 * 1024 * 1024);\n",
)
replace_once(
    "src/gateway.rs",
    "        assert_eq!(prepared.peer.options.max_h2_streams, 32);\n",
    "        assert_eq!(prepared.peer.options.max_h2_streams, 128);\n"
    "        assert_eq!(prepared.peer.options.h2_stream_window_size, Some(16 * 1024 * 1024));\n"
    "        assert_eq!(prepared.peer.options.h2_connection_window_size, Some(64 * 1024 * 1024));\n"
    "        assert_eq!(prepared.peer.options.h2_ping_interval, Some(Duration::from_secs(30)));\n",
)

# Trusted localhost H3 -> Pingora H2c bridge: reduce flow-control stalls without
# increasing the public/untrusted H2 windows.
replace_once(
    "src/main.rs",
    "        256 * 1024,\n        4 * 1024 * 1024,\n",
    "        2 * 1024 * 1024,\n        32 * 1024 * 1024,\n",
)
replace_once(
    "src/http3.rs",
    "        .http2_initial_stream_window_size(256 * 1024)\n"
    "        .http2_initial_connection_window_size(4 * 1024 * 1024)\n",
    "        .http2_initial_stream_window_size(2 * 1024 * 1024)\n"
    "        .http2_initial_connection_window_size(32 * 1024 * 1024)\n",
)

# High-capacity upstream QUIC only; public QUIC memory-defense windows stay unchanged.
replace_once("src/upstream_h3.rs", "const MAX_REQUEST_COMMANDS: usize = 256;\n", "const MAX_REQUEST_COMMANDS: usize = 512;\n")
replace_once("src/upstream_h3.rs", "const MAX_PENDING_REQUESTS: usize = 256;\n", "const MAX_PENDING_REQUESTS: usize = 512;\n")
replace_once("src/upstream_h3.rs", "const INITIAL_MAX_DATA: u64 = 64 * 1024 * 1024;\n", "const INITIAL_MAX_DATA: u64 = 128 * 1024 * 1024;\n")
replace_once("src/upstream_h3.rs", "const SEND_CAPACITY_FACTOR: f64 = 2.0;\n", "const SEND_CAPACITY_FACTOR: f64 = 3.0;\n")

# Production profile for the larger host. Explicit H1 origins keep their protocol.
replace_once("config/pingora.yaml", "  upstream_keepalive_pool_size: 128\n", "  upstream_keepalive_pool_size: 512\n")
yaml = read("config/pingora.yaml")
old_doh = '''    http2_max_concurrent_streams: 64
    sni: direct.tae00217.cloud
    verify_certificate: true
    connect_timeout_seconds: 2
    read_timeout_seconds: 30
    write_timeout_seconds: 30
    idle_timeout_seconds: 30'''
new_doh = '''    http2_max_concurrent_streams: 256
    http2_stream_window_bytes: 16777216
    http2_connection_window_bytes: 67108864
    http2_ping_interval_seconds: 30
    sni: direct.tae00217.cloud
    verify_certificate: true
    connect_timeout_seconds: 2
    read_timeout_seconds: 30
    write_timeout_seconds: 30
    idle_timeout_seconds: 300'''
if yaml.count(old_doh) != 2:
    raise SystemExit(f"config/pingora.yaml: expected two DoH blocks, found {yaml.count(old_doh)}")
write("config/pingora.yaml", yaml.replace(old_doh, new_doh))
