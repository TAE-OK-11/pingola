use std::fs::{self, File, Metadata};
use std::io::{BufReader, Read};
use std::net::SocketAddr;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use rustix::process::{getegid, geteuid};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use socket2::{Domain, Protocol, Socket, Type};

use crate::config::RuntimeConfig;

const MAX_PEM_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Debug)]
pub struct CheckItem {
    pub name: String,
    pub result: Result<String, String>,
}

#[derive(Debug, Default)]
pub struct CheckReport {
    pub items: Vec<CheckItem>,
}

impl CheckReport {
    pub fn ok(&mut self, name: impl Into<String>, detail: impl Into<String>) {
        self.items.push(CheckItem {
            name: name.into(),
            result: Ok(detail.into()),
        });
    }

    pub fn error(&mut self, name: impl Into<String>, error: impl Into<String>) {
        self.items.push(CheckItem {
            name: name.into(),
            result: Err(error.into()),
        });
    }

    pub fn is_ok(&self) -> bool {
        self.items.iter().all(|item| item.result.is_ok())
    }

    pub fn failures(&self) -> usize {
        self.items
            .iter()
            .filter(|item| item.result.is_err())
            .count()
    }

    pub fn print(&self) {
        for item in &self.items {
            match &item.result {
                Ok(detail) => println!("[ok] {}: {}", item.name, detail),
                Err(error) => eprintln!("[error] {}: {}", item.name, error),
            }
        }
        if self.is_ok() {
            println!("preflight summary: {} checks passed", self.items.len());
        } else {
            eprintln!(
                "preflight summary: {} of {} checks failed",
                self.failures(),
                self.items.len()
            );
        }
    }
}

pub fn check_runtime(runtime: &RuntimeConfig, check_bind: bool) -> CheckReport {
    let mut report = CheckReport::default();
    let server = &runtime.config.server;

    if server.https_listen.is_empty() {
        report.ok("TLS files", "not required (no HTTPS listeners)");
    } else {
        check_tls(runtime, &mut report);
    }

    for (name, host) in &runtime.config.hosts {
        let Some(root) = host.static_root.as_ref() else {
            continue;
        };
        match fs::read_dir(root) {
            Ok(_) => report.ok(
                format!("static_root {name}"),
                format!("readable directory path={}", root.display()),
            ),
            Err(error) => report.error(
                format!("static_root {name}"),
                format!("cannot read directory: {error}; {}", describe_file(root)),
            ),
        }
    }

    let socket_parent = server
        .health_socket
        .parent()
        .unwrap_or_else(|| Path::new("/"));
    match fs::metadata(socket_parent) {
        Ok(metadata) if metadata.is_dir() => report.ok(
            "health socket directory",
            format!("path={}", socket_parent.display()),
        ),
        Ok(_) => report.error(
            "health socket directory",
            format!("path={} is not a directory", socket_parent.display()),
        ),
        Err(error) => report.error(
            "health socket directory",
            format!("path={} error={error}", socket_parent.display()),
        ),
    }

    if check_bind {
        check_listener_binds(runtime, &mut report);
    } else {
        let count = server.http_listen.len() + server.https_listen.len();
        report.ok(
            "listener syntax",
            format!("{count} numeric socket addresses parsed; bind skipped"),
        );
    }

    report
}

fn check_tls(runtime: &RuntimeConfig, report: &mut CheckReport) {
    let server = &runtime.config.server;
    let certificate = server
        .certificate
        .as_deref()
        .expect("schema validation requires certificate for HTTPS");
    let private_key = server
        .private_key
        .as_deref()
        .expect("schema validation requires private_key for HTTPS");

    let certificate_bytes = check_file_read("certificate open", certificate, report);
    let private_key_bytes = check_file_read("private key open", private_key, report);

    let certificates = certificate_bytes
        .as_deref()
        .and_then(|bytes| parse_certificates(bytes, certificate, report));
    let key = private_key_bytes
        .as_deref()
        .and_then(|bytes| parse_private_key(bytes, private_key, report));

    if let (Some(certificates), Some(key)) = (certificates, key) {
        match rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(certificates, key)
        {
            Ok(_) => report.ok(
                "certificate/private key match",
                "rustls accepted the certificate chain and private key",
            ),
            Err(error) => report.error(
                "certificate/private key match",
                format!(
                    "TLS settings creation rejected certificate={} private_key={}: {error}",
                    certificate.display(),
                    private_key.display()
                ),
            ),
        }
    } else {
        report.error(
            "certificate/private key match",
            "not checked because one or both PEM files could not be parsed",
        );
    }
}

fn check_file_read(name: &str, path: &Path, report: &mut CheckReport) -> Option<Vec<u8>> {
    match read_limited(path) {
        Ok(bytes) => {
            report.ok(
                name,
                format!("readable bytes={}; {}", bytes.len(), describe_file(path)),
            );
            Some(bytes)
        }
        Err(error) => {
            report.error(name, format!("{error:#}; {}", describe_file(path)));
            None
        }
    }
}

fn read_limited(path: &Path) -> Result<Vec<u8>> {
    let file = File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let mut bytes = Vec::new();
    file.take(MAX_PEM_BYTES + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("failed to read {}", path.display()))?;
    if bytes.len() as u64 > MAX_PEM_BYTES {
        return Err(anyhow!(
            "PEM file {} exceeds {} bytes",
            path.display(),
            MAX_PEM_BYTES
        ));
    }
    Ok(bytes)
}

fn parse_certificates(
    bytes: &[u8],
    path: &Path,
    report: &mut CheckReport,
) -> Option<Vec<CertificateDer<'static>>> {
    let mut reader = BufReader::new(bytes);
    let parsed = rustls_pemfile::certs(&mut reader).collect::<std::io::Result<Vec<_>>>();
    match parsed {
        Ok(certificates) if !certificates.is_empty() => {
            report.ok(
                "certificate PEM parse",
                format!(
                    "path={} certificates={}",
                    path.display(),
                    certificates.len()
                ),
            );
            Some(certificates)
        }
        Ok(_) => {
            report.error(
                "certificate PEM parse",
                format!("path={} contains no certificates", path.display()),
            );
            None
        }
        Err(error) => {
            report.error(
                "certificate PEM parse",
                format!("path={} invalid PEM: {error}", path.display()),
            );
            None
        }
    }
}

fn parse_private_key(
    bytes: &[u8],
    path: &Path,
    report: &mut CheckReport,
) -> Option<PrivateKeyDer<'static>> {
    let mut reader = BufReader::new(bytes);
    match rustls_pemfile::private_key(&mut reader) {
        Ok(Some(key)) => {
            report.ok(
                "private key PEM parse",
                format!("path={} parsed (key material redacted)", path.display()),
            );
            Some(key)
        }
        Ok(None) => {
            report.error(
                "private key PEM parse",
                format!("path={} contains no supported private key", path.display()),
            );
            None
        }
        Err(error) => {
            report.error(
                "private key PEM parse",
                format!("path={} invalid PEM: {error}", path.display()),
            );
            None
        }
    }
}

fn check_listener_binds(runtime: &RuntimeConfig, report: &mut CheckReport) {
    let mut sockets = Vec::new();
    for (protocol, address) in runtime
        .config
        .server
        .http_listen
        .iter()
        .map(|address| ("HTTP", address))
        .chain(
            runtime
                .config
                .server
                .https_listen
                .iter()
                .map(|address| ("HTTPS", address)),
        )
    {
        match bind_listener(address) {
            Ok(socket) => {
                report.ok(
                    format!("listener bind {protocol} {address}"),
                    if address.starts_with('[') {
                        "bound with IPV6_V6ONLY=true".to_string()
                    } else {
                        "bound".to_string()
                    },
                );
                sockets.push(socket);
            }
            Err(error) => report.error(
                format!("listener bind {protocol} {address}"),
                format!("failed to bind conflicting address {address}: {error:#}"),
            ),
        }
    }
    drop(sockets);
}

fn bind_listener(address: &str) -> Result<Socket> {
    let address = address
        .parse::<SocketAddr>()
        .with_context(|| format!("invalid listener address {address}"))?;
    let domain = if address.is_ipv6() {
        Domain::IPV6
    } else {
        Domain::IPV4
    };
    let socket = Socket::new(domain, Type::STREAM, Some(Protocol::TCP))
        .with_context(|| format!("failed to create socket for {address}"))?;
    socket
        .set_reuse_address(true)
        .with_context(|| format!("failed to set SO_REUSEADDR for {address}"))?;
    if address.is_ipv6() {
        socket
            .set_only_v6(true)
            .with_context(|| format!("failed to set IPV6_V6ONLY for {address}"))?;
    }
    socket
        .bind(&address.into())
        .with_context(|| format!("bind failed for {address}"))?;
    socket
        .listen(1024)
        .with_context(|| format!("listen failed for {address}"))?;
    Ok(socket)
}

fn describe_file(path: &Path) -> String {
    let uid = geteuid().as_raw();
    let gid = getegid().as_raw();
    let symlink_metadata = fs::symlink_metadata(path).ok();
    let symlink = symlink_metadata
        .as_ref()
        .is_some_and(Metadata::file_type_is_symlink);
    let final_target = final_symlink_target(path);
    let target_metadata = fs::metadata(path).ok();
    let (owner_uid, owner_gid, mode) = target_metadata
        .as_ref()
        .or(symlink_metadata.as_ref())
        .map_or((None, None, None), |metadata| {
            (
                Some(metadata.uid()),
                Some(metadata.gid()),
                Some(metadata.mode() & 0o7777),
            )
        });
    format!(
        "path={} process_uid={} process_gid={} owner_uid={} owner_gid={} mode={} symlink={} final_target={} target_exists={}",
        path.display(),
        uid,
        gid,
        owner_uid.map_or_else(|| "unknown".into(), |value| value.to_string()),
        owner_gid.map_or_else(|| "unknown".into(), |value| value.to_string()),
        mode.map_or_else(|| "unknown".into(), |value| format!("{value:04o}")),
        symlink,
        final_target.display(),
        target_metadata.is_some(),
    )
}

fn final_symlink_target(path: &Path) -> PathBuf {
    let mut current = path.to_path_buf();
    for _ in 0..40 {
        let Ok(metadata) = fs::symlink_metadata(&current) else {
            break;
        };
        if !metadata.file_type().is_symlink() {
            if let Ok(canonical) = fs::canonicalize(&current) {
                current = canonical;
            }
            break;
        }
        let Ok(target) = fs::read_link(&current) else {
            break;
        };
        current = if target.is_absolute() {
            target
        } else {
            current
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join(target)
        };
    }
    current
}

trait MetadataSymlink {
    fn file_type_is_symlink(&self) -> bool;
}

impl MetadataSymlink for Metadata {
    fn file_type_is_symlink(&self) -> bool {
        self.file_type().is_symlink()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use tempfile::tempdir;

    #[test]
    fn broken_symlink_diagnostic_includes_target() {
        let directory = tempdir().unwrap();
        let link = directory.path().join("privkey.pem");
        symlink("../archive/privkey7.pem", &link).unwrap();
        let description = describe_file(&link);
        assert!(description.contains("symlink=true"));
        assert!(description.contains("privkey7.pem"));
        assert!(description.contains("target_exists=false"));
    }

    #[test]
    fn simultaneous_ipv4_ipv6_wildcard_binds_do_not_conflict() {
        let ipv4 = bind_listener("0.0.0.0:0").unwrap();
        let port = ipv4.local_addr().unwrap().as_socket().unwrap().port();
        let ipv6 = bind_listener(&format!("[::]:{port}")).unwrap();
        assert!(ipv6.local_addr().unwrap().as_socket().unwrap().is_ipv6());
    }
}
