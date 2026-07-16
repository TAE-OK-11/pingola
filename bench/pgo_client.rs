//! Dependency-free HTTP/1.1 keepalive client used only to train the PGO image build.

use std::env;
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

const MAX_HEADER_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy)]
enum BodyValidation {
    Any,
    Pattern,
}

enum Workload {
    AlternatingPattern,
    Fixed {
        path: String,
        expected_status: u16,
        expected_length: usize,
        body_validation: BodyValidation,
    },
}

struct TrainingConfig {
    port: u16,
    host: String,
    requests_per_thread: usize,
    workload: Workload,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("PGO training client failed: {error}");
        std::process::exit(1);
    }
}

fn run() -> io::Result<()> {
    let mut port = 19_080_u16;
    let mut threads = 1_usize;
    let mut requests_per_thread = 1_000_usize;
    let mut host = "pgo.test".to_owned();
    let mut path = None;
    let mut expected_status = 200_u16;
    let mut expected_length = None;
    let mut body_validation = BodyValidation::Pattern;
    let mut args = env::args().skip(1);
    while let Some(argument) = args.next() {
        let value = args.next().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{argument} needs a value"),
            )
        })?;
        match argument.as_str() {
            "--port" => port = parse_number(&value, "port")?,
            "--threads" => threads = parse_number(&value, "threads")?,
            "--requests-per-thread" => {
                requests_per_thread = parse_number(&value, "requests-per-thread")?;
            }
            "--host" => host = value,
            "--path" => path = Some(value),
            "--expected-status" => expected_status = parse_number(&value, "expected-status")?,
            "--expected-length" => expected_length = Some(parse_number(&value, "expected-length")?),
            "--body-validation" => {
                body_validation = match value.as_str() {
                    "any" => BodyValidation::Any,
                    "pattern" => BodyValidation::Pattern,
                    _ => {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "body-validation must be any or pattern",
                        ));
                    }
                };
            }
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("unknown argument: {argument}"),
                ));
            }
        }
    }
    if !(1..=64).contains(&threads) || requests_per_thread == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "threads must be 1..=64 and requests-per-thread must be positive",
        ));
    }
    if host.is_empty()
        || host.contains(['\r', '\n'])
        || path
            .as_ref()
            .is_some_and(|value: &String| !value.starts_with('/') || value.contains(['\r', '\n']))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "host/path is empty or unsafe",
        ));
    }
    let workload = match path {
        Some(path) => Workload::Fixed {
            path,
            expected_status,
            expected_length: expected_length.ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "fixed path requires --expected-length",
                )
            })?,
            body_validation,
        },
        None if expected_length.is_none() && expected_status == 200 => Workload::AlternatingPattern,
        None => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "status/length overrides require --path",
            ));
        }
    };
    let config = Arc::new(TrainingConfig {
        port,
        host,
        requests_per_thread,
        workload,
    });

    let mut handles = Vec::with_capacity(threads);
    for worker in 0..threads {
        let config = Arc::clone(&config);
        handles.push(thread::spawn(move || train(&config, worker)));
    }
    for handle in handles {
        handle
            .join()
            .map_err(|_| io::Error::other("PGO client worker panicked"))??;
    }
    Ok(())
}

fn parse_number<T>(value: &str, name: &str) -> io::Result<T>
where
    T: std::str::FromStr,
{
    value.parse().map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{name} is not a valid number"),
        )
    })
}

fn train(config: &TrainingConfig, worker: usize) -> io::Result<()> {
    let mut stream = TcpStream::connect(("127.0.0.1", config.port))?;
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;
    let mut buffered = Vec::with_capacity(16 * 1024);

    for request_index in 0..config.requests_per_thread {
        let (path, expected_status, expected_length, body_validation) = match &config.workload {
            Workload::AlternatingPattern => {
                let size = if (worker + request_index) % 4 == 0 {
                    4096
                } else {
                    64
                };
                (
                    if size == 4096 {
                        "/bytes/4096"
                    } else {
                        "/bytes/64"
                    },
                    200,
                    size,
                    BodyValidation::Pattern,
                )
            }
            Workload::Fixed {
                path,
                expected_status,
                expected_length,
                body_validation,
            } => (
                path.as_str(),
                *expected_status,
                *expected_length,
                *body_validation,
            ),
        };
        write!(
            stream,
            "GET {path} HTTP/1.1\r\nHost: {}\r\nAccept-Encoding: identity\r\nConnection: keep-alive\r\n\r\n",
            config.host
        )?;
        read_response(
            &mut stream,
            &mut buffered,
            expected_status,
            expected_length,
            body_validation,
        )?;
    }
    Ok(())
}

fn read_response(
    stream: &mut TcpStream,
    buffered: &mut Vec<u8>,
    expected_status: u16,
    expected_length: usize,
    body_validation: BodyValidation,
) -> io::Result<()> {
    let header_end = loop {
        if let Some(position) = buffered.windows(4).position(|part| part == b"\r\n\r\n") {
            break position;
        }
        if buffered.len() >= MAX_HEADER_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "response header exceeds 64 KiB",
            ));
        }
        read_more(stream, buffered)?;
    };

    let header = std::str::from_utf8(&buffered[..header_end])
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "response header is not UTF-8"))?;
    let expected_prefix = format!("HTTP/1.1 {expected_status} ");
    if !header.starts_with(&expected_prefix) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "unexpected response status: {}",
                header.lines().next().unwrap_or("empty")
            ),
        ));
    }
    let content_length = header
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())
                .flatten()
        })
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing Content-Length"))?;
    if content_length != expected_length {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "response body length does not match the training request",
        ));
    }

    let message_length = header_end + 4 + content_length;
    while buffered.len() < message_length {
        read_more(stream, buffered)?;
    }
    if matches!(body_validation, BodyValidation::Pattern)
        && buffered[header_end + 4..message_length]
            .iter()
            .enumerate()
            .any(|(index, byte)| *byte != index as u8)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "training response body was corrupted",
        ));
    }
    buffered.drain(..message_length);
    Ok(())
}

fn read_more(stream: &mut TcpStream, buffered: &mut Vec<u8>) -> io::Result<()> {
    let mut chunk = [0_u8; 16 * 1024];
    let read = stream.read(&mut chunk)?;
    if read == 0 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "server closed the PGO training connection",
        ));
    }
    buffered.extend_from_slice(&chunk[..read]);
    Ok(())
}
