//! Apply Linux kernel socket offload and tuning on live connections.
//!
//! kTLS is optional; these paths work on stock kernels without TLS record
//! offload: UDP GSO/GRO, TCP fast open, enlarged SO_*BUF, TCP_QUICKACK, and
//! TCP_NOTSENT_LOWAT for streaming.

use std::sync::{Arc, OnceLock};

use cloudflare_pingora::Result;
use tokio::net::{TcpSocket, UdpSocket};

use crate::kernel_offload::KernelOffloadReport;

/// Receive buffer for proxy TCP legs (upstream + downstream accepted sockets).
pub const PROXY_TCP_RCVBUF: usize = 256 * 1024;
/// Send buffer for proxy TCP legs.
pub const PROXY_TCP_SNDBUF: usize = 256 * 1024;
/// Flush partial TCP sends sooner on streaming responses (Linux 4.6+).
pub const PROXY_TCP_NOTSENT_LOWAT: u32 = 16 * 1024;

type UpstreamTcpHook = Arc<dyn Fn(&TcpSocket) -> Result<()> + Send + Sync>;

static OFFLOAD_REPORT: OnceLock<KernelOffloadReport> = OnceLock::new();
static UPSTREAM_TCP_HOOK: OnceLock<UpstreamTcpHook> = OnceLock::new();

pub fn offload_report() -> &'static KernelOffloadReport {
    OFFLOAD_REPORT.get_or_init(KernelOffloadReport::probe)
}

pub fn log_active_offloads() {
    let report = offload_report();
    report.log_startup();
    if report.linux {
        log::info!(
            "kernel socket tuning: rcvbuf={} sndbuf={} notsent_lowat={} upstream_tfo={} downstream_tfo=backlog-64",
            PROXY_TCP_RCVBUF,
            PROXY_TCP_SNDBUF,
            PROXY_TCP_NOTSENT_LOWAT,
            yes_no(report.tcp_fastopen_client),
        );
    }
}

pub fn upstream_tcp_hook() -> UpstreamTcpHook {
    UPSTREAM_TCP_HOOK
        .get_or_init(|| Arc::new(tune_upstream_tcp))
        .clone()
}

pub fn tune_upstream_tcp(socket: &TcpSocket) -> Result<()> {
    tune_tcp_socket(socket)
}

pub fn apply_upstream_udp_offload(socket: &UdpSocket) -> String {
    #[cfg(target_os = "linux")]
    {
        let capabilities =
            tokio_quiche::socket::SocketCapabilities::apply_all_and_get_compatibility(socket);
        format!("{capabilities:?}")
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = socket;
        "n/a".to_string()
    }
}

fn tune_tcp_socket(socket: &TcpSocket) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        tune_tcp_fd(socket.as_raw_fd())?;
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn tune_tcp_fd(fd: std::os::unix::io::RawFd) -> Result<()> {
    use cloudflare_pingora::ErrorType::ConnectError;
    use cloudflare_pingora::OrErr;

    const TCP_QUICKACK: libc::c_int = 12;
    const TCP_NOTSENT_LOWAT: libc::c_int = 25;

    set_sockopt(
        fd,
        libc::SOL_TCP,
        TCP_QUICKACK,
        &1_i32 as *const _ as *const libc::c_void,
        std::mem::size_of::<i32>() as libc::socklen_t,
    )
    .or_err(ConnectError, "failed to set TCP_QUICKACK")?;

    let rcv = i32::try_from(PROXY_TCP_RCVBUF).unwrap_or(i32::MAX);
    set_sockopt(
        fd,
        libc::SOL_SOCKET,
        libc::SO_RCVBUF,
        &rcv as *const _ as *const libc::c_void,
        std::mem::size_of::<i32>() as libc::socklen_t,
    )
    .or_err(ConnectError, "failed to set SO_RCVBUF")?;

    let snd = i32::try_from(PROXY_TCP_SNDBUF).unwrap_or(i32::MAX);
    set_sockopt(
        fd,
        libc::SOL_SOCKET,
        libc::SO_SNDBUF,
        &snd as *const _ as *const libc::c_void,
        std::mem::size_of::<i32>() as libc::socklen_t,
    )
    .or_err(ConnectError, "failed to set SO_SNDBUF")?;

    let lowat = PROXY_TCP_NOTSENT_LOWAT;
    set_sockopt(
        fd,
        libc::SOL_TCP,
        TCP_NOTSENT_LOWAT,
        &lowat as *const _ as *const libc::c_void,
        std::mem::size_of::<u32>() as libc::socklen_t,
    )
    .or_err(ConnectError, "failed to set TCP_NOTSENT_LOWAT")?;

    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn tune_tcp_fd(_fd: std::os::unix::io::RawFd) -> Result<()> {
    Ok(())
}

#[cfg(target_os = "linux")]
fn set_sockopt(
    fd: std::os::unix::io::RawFd,
    level: libc::c_int,
    opt: libc::c_int,
    value: *const libc::c_void,
    len: libc::socklen_t,
) -> std::io::Result<()> {
    let ret = unsafe { libc::setsockopt(fd, level, opt, value, len) };
    if ret == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

fn yes_no(value: bool) -> &'static str {
    if value { "on" } else { "off" }
}
