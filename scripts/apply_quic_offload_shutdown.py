#!/usr/bin/env python3
from pathlib import Path


def replace_once(path: str, old: str, new: str) -> None:
    p = Path(path)
    text = p.read_text()
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{path}: expected exactly one match, found {count}: {old[:80]!r}")
    p.write_text(text.replace(old, new, 1))


replace_once(
    "Cargo.toml",
    'tokio-quiche = "=0.19.1"',
    'tokio-quiche = { version = "=0.19.1", default-features = false }',
)

replace_once(
    "src/http3.rs",
    'use tokio_quiche::settings::{CertificateKind, Hooks, QuicSettings, TlsCertificatePaths};\nuse tokio_quiche::{ConnectionParams, ServerH3Driver, listen};',
    'use tokio_quiche::settings::{CertificateKind, Hooks, QuicSettings, TlsCertificatePaths};\nuse tokio_quiche::socket::QuicListener;\nuse tokio_quiche::{ConnectionParams, ServerH3Driver, listen_with_capabilities};',
)

replace_once(
    "src/http3.rs",
    '''    let mut sockets = Vec::with_capacity(server.http3_listen.len());
    for address in &server.http3_listen {
        let socket = UdpSocket::bind(address)
            .await
            .with_context(|| format!("failed to bind HTTP/3 UDP listener {address}"))?;
        sockets.push(socket);
    }
''',
    '''    let mut quic_listeners = Vec::with_capacity(server.http3_listen.len());
    for address in &server.http3_listen {
        let socket = UdpSocket::bind(address)
            .await
            .with_context(|| format!("failed to bind HTTP/3 UDP listener {address}"))?;
        let mut listener = QuicListener::try_from(socket)
            .with_context(|| format!("failed to prepare HTTP/3 QUIC listener {address}"))?;
        // tokio-quiche listeners default every socket capability to OFF. On Linux
        // this best-effort probe enables the supported subset of UDP GSO/GRO,
        // SO_TXTIME pacing, RX queue overflow accounting, and PMTU probe sockopts.
        // Unsupported kernel/NIC capabilities remain disabled instead of failing
        // startup, so the same image stays portable across hosts.
        listener.apply_max_capabilities();
        info!(
            "HTTP/3 UDP offload capabilities: address={} capabilities={:?}",
            address, listener.capabilities
        );
        quic_listeners.push(listener);
    }
''',
)

replace_once(
    "src/http3.rs",
    '''    quic.discover_path_mtu = true;
    // Keep QUIC packet sends paced instead of bursty.
    quic.enable_pacing = true;
    quic.send_capacity_factor = HTTP3_SEND_CAPACITY_FACTOR;
''',
    '''    quic.discover_path_mtu = true;
    quic.pmtud_max_probes = 3;
    // Keep QUIC packet sends paced instead of bursty. With the listener socket
    // capabilities enabled above, tokio-quiche can use SO_TXTIME where Linux
    // supports it and falls back to userspace pacing otherwise.
    quic.enable_pacing = true;
    quic.enable_hystart = true;
    quic.send_capacity_factor = HTTP3_SEND_CAPACITY_FACTOR;
''',
)

replace_once(
    "src/http3.rs",
    '''    quic.disable_active_migration = true;
    // Stateless Retry proves source-address ownership before the server allocates
''',
    '''    quic.disable_active_migration = true;
    // Keep connection-ID/path state minimal. NAT rebinding can still be handled
    // sequentially while an attacker cannot queue multiple PATH_CHALLENGE frames.
    quic.active_connection_id_limit = 2;
    quic.max_path_challenge_recv_queue_len = 1;
    quic.grease = true;
    // Stateless Retry proves source-address ownership before the server allocates
''',
)

replace_once(
    "src/http3.rs",
    '''    let listeners = listen(sockets, params, DefaultMetrics)
        .context("failed to create quiche HTTP/3 listeners")?;
''',
    '''    let listeners = listen_with_capabilities(quic_listeners, params, DefaultMetrics)
        .context("failed to create quiche HTTP/3 listeners with UDP offload capabilities")?;
''',
)

replace_once(
    "src/http3.rs",
    '"HTTP/3 frontend started: udp={:?} internal=h2c://{} quiche={} hybrid_pq={} stateless_retry=true max_amplification={} early_data=false migration=false pmtud=true pacing=true max_udp_payload={} send_capacity_factor={} admission_rate={}/s burst={} max_connections_per_ip={} handshake_timeout={}s",',
    '"HTTP/3 frontend started: udp={:?} internal=h2c://{} quiche={} hybrid_pq={} stateless_retry=true max_amplification={} early_data=false migration=false pmtud=true pacing=true socket_offload=auto[gso,gro,so_txtime,rxq_ovfl,pmtu_probe] max_udp_payload={} send_capacity_factor={} admission_rate={}/s burst={} max_connections_per_ip={} handshake_timeout={}s",',
)

replace_once(
    "src/config.rs",
    '''fn default_graceful_shutdown() -> u64 {
    60
}
''',
    '''fn default_graceful_shutdown() -> u64 {
    // Container replacement should stop accepting new work immediately and only
    // spend a short bounded interval draining in-flight requests.
    5
}
''',
)

replace_once(
    "config/pingora.yaml",
    "  graceful_shutdown_timeout_seconds: 60\n",
    "  graceful_shutdown_timeout_seconds: 5\n",
)

replace_once(
    "docker-compose.yml",
    "    stop_grace_period: 65s\n",
    "    stop_signal: SIGTERM\n    stop_grace_period: 8s\n",
)

replace_once(
    "Dockerfile",
    '''EXPOSE 80/tcp 443/tcp 443/udp

HEALTHCHECK''',
    '''EXPOSE 80/tcp 443/tcp 443/udp

# Pingora handles SIGTERM as a graceful shutdown request. Keep the image's stop
# contract explicit for Docker/Compose and other OCI runtimes.
STOPSIGNAL SIGTERM

HEALTHCHECK''',
)

replace_once(
    "README.md",
    '''Compose의
`stop_grace_period: 65s`는 기본 60초 graceful drain보다 길게 잡혀 Docker가 재생 중인
stream을 10초 기본 timeout으로 강제 종료하지 않게 하며, file descriptor limit은
32,768, process/thread 수는 256으로 명시해 자원 증가를 bounded 상태로 유지합니다.''',
    '''Compose는 `SIGTERM`을 명시적으로 사용하고 `stop_grace_period: 8s`를 둡니다.
Pingora는 새 연결을 즉시 받지 않은 뒤 기본 5초 동안만 in-flight 요청을 graceful drain하므로
이미지 업데이트가 60초씩 지연되지 않습니다. 8초는 Pingora drain 뒤 정리할 여유를 주며,
file descriptor limit은 32,768, process/thread 수는 256으로 명시해 자원 증가를 bounded
상태로 유지합니다.''',
)
