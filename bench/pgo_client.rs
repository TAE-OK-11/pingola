//! Dependency-free HTTP/1.1 keepalive client used only to train the PGO image build.

use std::env;
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::thread;
use std::time::Duration;

const MAX_HEADER_BYTES: usize = 64 * 1024;

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

    let mut handles = Vec::with_capacity(threads);
    for worker in 0..threads {
        handles.push(thread::spawn(move || {
            train(port, worker, requests_per_thread)
        }));
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

fn train(port: u16, worker: usize, requests: usize) -> io::Result<()> {
    let mut stream = TcpStream::connect(("127.0.0.1", port))?;
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;
    let mut buffered = Vec::with_capacity(16 * 1024);

    for request_index in 0..requests {
        let size = if (worker + request_index) % 4 == 0 {
            4096
        } else {
            64
        };
        write!(
            stream,
            "GET /bytes/{size} HTTP/1.1\r\nHost: pgo.test\r\nAccept-Encoding: identity\r\nConnection: keep-alive\r\n\r\n"
        )?;
        read_response(&mut stream, &mut buffered, size)?;
    }
    Ok(())
}

fn read_response(
    stream: &mut TcpStream,
    buffered: &mut Vec<u8>,
    expected_length: usize,
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
    if !header.starts_with("HTTP/1.1 200 ") {
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
    if buffered[header_end + 4..message_length]
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
