//! Runtime probes for Linux kernel offload features used by the gateway.
//!
//! Docker images inherit the **host** kernel. These checks document what the
//! current pod can use (kTLS, UDP GSO/GRO, TCP fast open) and surface gaps in
//! `--check` output before production traffic hits the path.

use std::fmt::Write as _;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KernelOffloadReport {
    pub linux: bool,
    pub ktls_ulp: bool,
    pub udp_gso: bool,
    pub udp_gro: bool,
    pub tcp_fastopen_client: bool,
}

impl KernelOffloadReport {
    pub fn probe() -> Self {
        Self {
            linux: cfg!(target_os = "linux"),
            ktls_ulp: probe_ktls_ulp(),
            udp_gso: probe_udp_segment(),
            udp_gro: probe_udp_gro(),
            tcp_fastopen_client: probe_tcp_fastopen_client(),
        }
    }

    pub fn summary(&self) -> String {
        let mut out = String::with_capacity(160);
        if !self.linux {
            let _ = write!(out, "non-linux host");
            return out;
        }
        let _ = write!(
            out,
            "ktls_ulp={} udp_gso={} udp_gro={} tcp_fastopen_client={}",
            yes_no(self.ktls_ulp),
            yes_no(self.udp_gso),
            yes_no(self.udp_gro),
            yes_no(self.tcp_fastopen_client),
        );
        out
    }

    pub fn log_startup(&self) {
        if !self.linux {
            log::info!("kernel offload: non-linux host; userspace TLS/QUIC only");
            return;
        }
        log::info!("kernel offload: {}", self.summary());
        if !self.ktls_ulp {
            log::info!(
                "kernel offload: kTLS ULP unavailable; TCP TLS stays in userspace BoringSSL (H3/QUIC unaffected)"
            );
        }
        if !self.udp_gso || !self.udp_gro {
            log::info!(
                "kernel offload: UDP segmentation offload partially unavailable; HTTP/3 falls back to userspace framing"
            );
        }
    }
}

fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

fn probe_ktls_ulp() -> bool {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::io::AsRawFd;

        use socket2::{Domain, Socket, Type};

        const TCP_ULP: libc::c_int = 31;
        let Ok(socket) = Socket::new(Domain::IPV4, Type::STREAM, None) else {
            return false;
        };
        let ulp = b"tls\0";
        let ret = unsafe {
            libc::setsockopt(
                socket.as_raw_fd(),
                libc::SOL_TCP,
                TCP_ULP,
                ulp.as_ptr().cast(),
                ulp.len() as libc::socklen_t,
            )
        };
        ret == 0
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

fn probe_udp_segment() -> bool {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::io::AsRawFd;

        use socket2::{Domain, Socket, Type};

        const UDP_SEGMENT: libc::c_int = 103;
        let Ok(socket) = Socket::new(Domain::IPV4, Type::DGRAM, None) else {
            return false;
        };
        let value: libc::c_int = 1;
        let ret = unsafe {
            libc::setsockopt(
                socket.as_raw_fd(),
                libc::SOL_UDP,
                UDP_SEGMENT,
                &value as *const _ as *const libc::c_void,
                std::mem::size_of_val(&value) as libc::socklen_t,
            )
        };
        ret == 0
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

fn probe_udp_gro() -> bool {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::io::AsRawFd;

        use socket2::{Domain, Socket, Type};

        const UDP_GRO: libc::c_int = 104;
        let Ok(socket) = Socket::new(Domain::IPV4, Type::DGRAM, None) else {
            return false;
        };
        let value: libc::c_int = 1;
        let ret = unsafe {
            libc::setsockopt(
                socket.as_raw_fd(),
                libc::SOL_UDP,
                UDP_GRO,
                &value as *const _ as *const libc::c_void,
                std::mem::size_of_val(&value) as libc::socklen_t,
            )
        };
        ret == 0
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

fn probe_tcp_fastopen_client() -> bool {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_to_string("/proc/sys/net/ipv4/tcp_fastopen")
            .ok()
            .and_then(|value| value.trim().parse::<u32>().ok())
            .is_some_and(|value| value & 0b01 != 0)
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_report_formats_summary() {
        let report = KernelOffloadReport::probe();
        let summary = report.summary();
        if report.linux {
            assert!(summary.contains("ktls_ulp="));
            assert!(summary.contains("udp_gso="));
        } else {
            assert_eq!(summary, "non-linux host");
        }
    }
}
