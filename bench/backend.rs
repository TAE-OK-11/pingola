//! Dependency-free synthetic HTTP/1.1 backend for proxy correctness and benchmarks.
//!
//! This intentionally uses only the Rust standard library so benchmark hosts can
//! build it with `rustc` without resolving or downloading Cargo dependencies.

use std::collections::HashMap;
use std::env;
use std::fmt::Write as _;
use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::Duration;

const DEFAULT_LISTEN: &str = "127.0.0.1";
const DEFAULT_PORT: u16 = 18_700;
const MAX_HEADER_BYTES: usize = 64 * 1024;
const MAX_BODY_BYTES: usize = 512 * 1024 * 1024;
const MAX_PAYLOAD_BYTES: usize = 1024 * 1024 * 1024;
const MAX_CONNECTIONS: usize = 512;
const CONNECTION_STACK_BYTES: usize = 256 * 1024;
const STREAM_CHUNK_BYTES: usize = 16 * 1024;
const SMALL_RESPONSE_BYTES: usize = 16 * 1024;

static ACTIVE_CONNECTIONS: AtomicUsize = AtomicUsize::new(0);
static TOTAL_CONNECTIONS: AtomicUsize = AtomicUsize::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Method {
    Get,
    Head,
    Post,
    Other,
}

#[derive(Debug)]
struct Request {
    method: Method,
    target: String,
    range: Option<String>,
    close: bool,
    body: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ByteRange {
    start: usize,
    end: usize,
}

struct ConnectionGuard;

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        ACTIVE_CONNECTIONS.fetch_sub(1, Ordering::Relaxed);
    }
}

fn main() {
    if let Err(error) = run() {
        eprintln!("synthetic backend failed: {error}");
        std::process::exit(1);
    }
}

fn run() -> io::Result<()> {
    let (listen, port) = parse_args()?;
    let address = if listen.contains(':') && !listen.starts_with('[') {
        format!("[{listen}]:{port}")
    } else {
        format!("{listen}:{port}")
    };
    let listener = TcpListener::bind(&address)?;
    eprintln!("Rust synthetic backend listening on {address}");

    for accepted in listener.incoming() {
        let stream = match accepted {
            Ok(stream) => stream,
            Err(error) => {
                eprintln!("backend accept failed: {error}");
                thread::sleep(Duration::from_millis(10));
                continue;
            }
        };
        TOTAL_CONNECTIONS.fetch_add(1, Ordering::Relaxed);
        let active = ACTIVE_CONNECTIONS.fetch_add(1, Ordering::Relaxed) + 1;
        if active > MAX_CONNECTIONS {
            ACTIVE_CONNECTIONS.fetch_sub(1, Ordering::Relaxed);
            let _ = stream.shutdown(Shutdown::Both);
            continue;
        }
        let _ = stream.set_nodelay(true);
        if thread::Builder::new()
            .name("bench-backend".to_owned())
            .stack_size(CONNECTION_STACK_BYTES)
            .spawn(move || {
                let _guard = ConnectionGuard;
                let _ = serve_connection(stream);
            })
            .is_err()
        {
            ACTIVE_CONNECTIONS.fetch_sub(1, Ordering::Relaxed);
        }
    }
    Ok(())
}

fn parse_args() -> io::Result<(String, u16)> {
    let mut listen = DEFAULT_LISTEN.to_owned();
    let mut port = DEFAULT_PORT;
    let mut args = env::args().skip(1);
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--listen" => {
                listen = args.next().ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "--listen requires a value")
                })?;
            }
            "--port" => {
                let value = args.next().ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "--port requires a value")
                })?;
                port = value.parse().map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidInput, "--port must be a valid u16")
                })?;
            }
            "-h" | "--help" => {
                println!("usage: backend [--listen ADDRESS] [--port PORT]");
                std::process::exit(0);
            }
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("unknown argument: {argument}"),
                ));
            }
        }
    }
    Ok((listen, port))
}

fn serve_connection(mut stream: TcpStream) -> io::Result<()> {
    let mut buffered = Vec::with_capacity(16 * 1024);
    while let Some(request) = read_request(&mut stream, &mut buffered)? {
        let close = request.close;
        if let Err(error) = respond(&mut stream, request) {
            if matches!(
                error.kind(),
                io::ErrorKind::BrokenPipe
                    | io::ErrorKind::ConnectionAborted
                    | io::ErrorKind::ConnectionReset
                    | io::ErrorKind::UnexpectedEof
            ) {
                return Ok(());
            }
            return Err(error);
        }
        if close {
            return Ok(());
        }
    }
    Ok(())
}

fn read_request(stream: &mut TcpStream, buffered: &mut Vec<u8>) -> io::Result<Option<Request>> {
    let header_end = loop {
        if let Some(position) = find_header_end(buffered) {
            break position;
        }
        if buffered.len() >= MAX_HEADER_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "request headers exceed 64 KiB",
            ));
        }
        let mut chunk = [0_u8; 16 * 1024];
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            if buffered.is_empty() {
                return Ok(None);
            }
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "connection closed during request headers",
            ));
        }
        buffered.extend_from_slice(&chunk[..read]);
    };

    let headers = std::str::from_utf8(&buffered[..header_end])
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "request headers are not UTF-8"))?;
    let mut lines = headers.split("\r\n");
    let request_line = lines
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing request line"))?;
    let mut request_parts = request_line.split_whitespace();
    let method = match request_parts.next() {
        Some("GET") => Method::Get,
        Some("HEAD") => Method::Head,
        Some("POST") => Method::Post,
        Some(_) => Method::Other,
        None => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "missing request method",
            ));
        }
    };
    let target = request_parts
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing request target"))?
        .to_owned();
    let version = request_parts
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing HTTP version"))?;

    let mut content_length = 0_usize;
    let mut range = None;
    let mut connection_close = version.eq_ignore_ascii_case("HTTP/1.0");
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        if name.eq_ignore_ascii_case("content-length") {
            content_length = value.parse().map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "invalid Content-Length")
            })?;
            if content_length > MAX_BODY_BYTES {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "request body exceeds 512 MiB",
                ));
            }
        } else if name.eq_ignore_ascii_case("range") {
            range = Some(value.to_owned());
        } else if name.eq_ignore_ascii_case("connection") {
            connection_close = value
                .split(',')
                .any(|token| token.trim().eq_ignore_ascii_case("close"));
            if version.eq_ignore_ascii_case("HTTP/1.0")
                && value
                    .split(',')
                    .any(|token| token.trim().eq_ignore_ascii_case("keep-alive"))
            {
                connection_close = false;
            }
        }
    }

    let message_end = header_end + 4 + content_length;
    while buffered.len() < message_end {
        let mut chunk = [0_u8; 16 * 1024];
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "connection closed during request body",
            ));
        }
        buffered.extend_from_slice(&chunk[..read]);
    }
    let body = buffered[header_end + 4..message_end].to_vec();
    buffered.drain(..message_end);
    Ok(Some(Request {
        method,
        target,
        range,
        close: connection_close,
        body,
    }))
}

fn find_header_end(input: &[u8]) -> Option<usize> {
    input.windows(4).position(|window| window == b"\r\n\r\n")
}

fn respond(stream: &mut TcpStream, request: Request) -> io::Result<()> {
    if request.method == Method::Post {
        return respond_post(stream, &request.body, request.close);
    }
    if request.method == Method::Other {
        return write_text_response(
            stream,
            405,
            "Method Not Allowed",
            b"method not allowed\n",
            request.close,
        );
    }

    let send_body = request.method == Method::Get;
    let path = request.target.split('?').next().unwrap_or("/");
    if path == "/health" || path == "/status/204" {
        return write_empty_response(stream, 204, "No Content", request.close);
    }
    if path == "/stats/connections" {
        let body = format!("{}\n", TOTAL_CONNECTIONS.load(Ordering::Relaxed));
        return write_text_response(stream, 200, "OK", body.as_bytes(), request.close);
    }
    if path == "/status/500" {
        return write_text_response(
            stream,
            500,
            "Internal Server Error",
            b"backend error\n",
            request.close,
        );
    }
    if path == "/reset" {
        let _ = stream.shutdown(Shutdown::Both);
        return Err(io::Error::new(
            io::ErrorKind::ConnectionReset,
            "intentional backend reset",
        ));
    }
    if let Some(value) = path.strip_prefix("/pause/") {
        let size = parse_payload_size(value)?;
        return write_chunked_response(stream, size, send_body, true, request.close);
    }
    if let Some(value) = path.strip_prefix("/stream/") {
        let size = parse_payload_size(value)?;
        return write_chunked_response(stream, size, send_body, false, request.close);
    }
    if let Some(value) = path.strip_prefix("/json/") {
        let size = parse_payload_size(value)?;
        return write_json_response(stream, size, send_body, request.close);
    }
    if let Some(value) = path.strip_prefix("/bytes/") {
        let size = parse_payload_size(value)?;
        return write_bytes_response(
            stream,
            size,
            request.range.as_deref(),
            send_body,
            request.close,
        );
    }
    write_text_response(stream, 404, "Not Found", b"not found\n", request.close)
}

fn write_json_response(
    stream: &mut TcpStream,
    size: usize,
    send_body: bool,
    close: bool,
) -> io::Result<()> {
    const PREFIX: &[u8] = b"{\"data\":\"";
    const SUFFIX: &[u8] = b"\"}\n";
    if size < PREFIX.len() + SUFFIX.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "JSON payload must be at least 12 bytes",
        ));
    }

    let mut response = Vec::with_capacity(192 + size.min(SMALL_RESPONSE_BYTES));
    write!(
        response,
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {size}\r\n{}\r\n",
        connection_header(close)
    )
    .expect("writing to Vec cannot fail");
    if !send_body {
        return stream.write_all(&response);
    }

    let fill = size - PREFIX.len() - SUFFIX.len();
    if size <= SMALL_RESPONSE_BYTES {
        response.extend_from_slice(PREFIX);
        response.resize(response.len() + fill, b'a');
        response.extend_from_slice(SUFFIX);
        return stream.write_all(&response);
    }

    stream.write_all(&response)?;
    stream.write_all(PREFIX)?;
    let chunk = [b'a'; STREAM_CHUNK_BYTES];
    let mut remaining = fill;
    while remaining > 0 {
        let count = remaining.min(chunk.len());
        stream.write_all(&chunk[..count])?;
        remaining -= count;
    }
    stream.write_all(SUFFIX)
}

fn parse_payload_size(value: &str) -> io::Result<usize> {
    let size = value
        .parse::<usize>()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid payload size"))?;
    if size > MAX_PAYLOAD_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "payload exceeds 1 GiB",
        ));
    }
    Ok(size)
}

fn respond_post(stream: &mut TcpStream, body: &[u8], close: bool) -> io::Result<()> {
    let mut response = Vec::with_capacity(192 + body.len().min(SMALL_RESPONSE_BYTES));
    write!(
        response,
        "HTTP/1.1 200 OK\r\nContent-Type: application/dns-message\r\nContent-Length: {}\r\nCache-Control: max-age=3600\r\n{}\r\n",
        body.len(),
        connection_header(close)
    )
    .expect("writing to Vec cannot fail");
    if body.len() <= SMALL_RESPONSE_BYTES {
        response.extend_from_slice(body);
        stream.write_all(&response)
    } else {
        stream.write_all(&response)?;
        stream.write_all(body)
    }
}

fn write_bytes_response(
    stream: &mut TcpStream,
    size: usize,
    requested_range: Option<&str>,
    send_body: bool,
    close: bool,
) -> io::Result<()> {
    let selected = match requested_range {
        Some(value) => match parse_byte_range(value, size) {
            Some(range) => Some(range),
            None => return write_range_not_satisfiable(stream, size, close),
        },
        None => None,
    };
    let (status, reason, start, length, content_range) = if let Some(range) = selected {
        (
            206,
            "Partial Content",
            range.start,
            range.end - range.start + 1,
            Some(format!("bytes {}-{}/{}", range.start, range.end, size)),
        )
    } else {
        (200, "OK", 0, size, None)
    };
    let etag = payload_etag(size);
    let mut response = Vec::with_capacity(256 + length.min(SMALL_RESPONSE_BYTES));
    write!(
        response,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/octet-stream\r\nContent-Length: {length}\r\nAccept-Ranges: bytes\r\nETag: {etag}\r\n"
    )
    .expect("writing to Vec cannot fail");
    if let Some(value) = content_range {
        write!(response, "Content-Range: {value}\r\n").expect("writing to Vec cannot fail");
    }
    write!(response, "{}\r\n", connection_header(close)).expect("writing to Vec cannot fail");

    if !send_body {
        return stream.write_all(&response);
    }
    if length <= SMALL_RESPONSE_BYTES {
        append_pattern(&mut response, start, length);
        stream.write_all(&response)
    } else {
        stream.write_all(&response)?;
        write_pattern(stream, start, length)
    }
}

fn write_chunked_response(
    stream: &mut TcpStream,
    size: usize,
    send_body: bool,
    pause: bool,
    close: bool,
) -> io::Result<()> {
    let mut response = Vec::with_capacity(160);
    write!(
        response,
        "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nTransfer-Encoding: chunked\r\n{}\r\n",
        connection_header(close)
    )
    .expect("writing to Vec cannot fail");
    stream.write_all(&response)?;
    if !send_body {
        return Ok(());
    }
    if pause {
        let midpoint = size / 2;
        write_chunk(stream, 0, midpoint)?;
        thread::sleep(Duration::from_secs(1));
        write_chunk(stream, midpoint, size - midpoint)?;
    } else {
        let mut offset = 0;
        while offset < size {
            let length = (size - offset).min(STREAM_CHUNK_BYTES);
            write_chunk(stream, offset, length)?;
            offset += length;
        }
    }
    stream.write_all(b"0\r\n\r\n")
}

fn write_chunk(stream: &mut TcpStream, offset: usize, length: usize) -> io::Result<()> {
    if length == 0 {
        return Ok(());
    }
    let mut prefix = String::with_capacity(24);
    write!(prefix, "{length:x}\r\n").expect("writing to String cannot fail");
    stream.write_all(prefix.as_bytes())?;
    write_pattern(stream, offset, length)?;
    stream.write_all(b"\r\n")
}

fn write_pattern(stream: &mut TcpStream, mut offset: usize, mut length: usize) -> io::Result<()> {
    let pattern = pattern_block();
    while length > 0 {
        let position = offset % pattern.len();
        let count = length.min(pattern.len() - position);
        stream.write_all(&pattern[position..position + count])?;
        offset += count;
        length -= count;
    }
    Ok(())
}

fn append_pattern(output: &mut Vec<u8>, mut offset: usize, mut length: usize) {
    let pattern = pattern_block();
    while length > 0 {
        let position = offset % pattern.len();
        let count = length.min(pattern.len() - position);
        output.extend_from_slice(&pattern[position..position + count]);
        offset += count;
        length -= count;
    }
}

fn pattern_block() -> &'static [u8; 65_536] {
    static PATTERN: OnceLock<[u8; 65_536]> = OnceLock::new();
    PATTERN.get_or_init(|| std::array::from_fn(|index| index as u8))
}

fn write_empty_response(
    stream: &mut TcpStream,
    status: u16,
    reason: &str,
    close: bool,
) -> io::Result<()> {
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\n{}\r\n",
        connection_header(close)
    );
    stream.write_all(response.as_bytes())
}

fn write_text_response(
    stream: &mut TcpStream,
    status: u16,
    reason: &str,
    body: &[u8],
    close: bool,
) -> io::Result<()> {
    let mut response = Vec::with_capacity(160 + body.len());
    write!(
        response,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n{}\r\n",
        body.len(),
        connection_header(close)
    )
    .expect("writing to Vec cannot fail");
    response.extend_from_slice(body);
    stream.write_all(&response)
}

fn write_range_not_satisfiable(stream: &mut TcpStream, size: usize, close: bool) -> io::Result<()> {
    let response = format!(
        "HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */{size}\r\nContent-Length: 0\r\n{}\r\n",
        connection_header(close)
    );
    stream.write_all(response.as_bytes())
}

fn connection_header(close: bool) -> &'static str {
    if close {
        "Connection: close\r\n"
    } else {
        ""
    }
}

fn parse_byte_range(value: &str, size: usize) -> Option<ByteRange> {
    if size == 0 {
        return None;
    }
    let specification = value.trim().strip_prefix("bytes=")?;
    if specification.contains(',') {
        return None;
    }
    let (first, last) = specification.split_once('-')?;
    if first.is_empty() {
        let suffix = last.parse::<usize>().ok()?;
        if suffix == 0 {
            return None;
        }
        let length = suffix.min(size);
        return Some(ByteRange {
            start: size - length,
            end: size - 1,
        });
    }
    let start = first.parse::<usize>().ok()?;
    if start >= size {
        return None;
    }
    let end = if last.is_empty() {
        size - 1
    } else {
        last.parse::<usize>().ok()?.min(size - 1)
    };
    if end < start {
        return None;
    }
    Some(ByteRange { start, end })
}

fn payload_etag(size: usize) -> String {
    static ETAGS: OnceLock<Mutex<HashMap<usize, String>>> = OnceLock::new();
    let cache = ETAGS.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(value) = cache
        .lock()
        .expect("ETag cache mutex poisoned")
        .get(&size)
        .cloned()
    {
        return value;
    }
    let mut hasher = Sha256::new();
    let mut remaining = size;
    let pattern = pattern_block();
    while remaining > 0 {
        let count = remaining.min(pattern.len());
        hasher.update(&pattern[..count]);
        remaining -= count;
    }
    let digest = hasher.finalize();
    let mut value = String::with_capacity(73);
    value.push_str("\"sha256-");
    for byte in digest {
        write!(value, "{byte:02x}").expect("writing to String cannot fail");
    }
    value.push('"');
    cache
        .lock()
        .expect("ETag cache mutex poisoned")
        .entry(size)
        .or_insert_with(|| value.clone())
        .clone()
}

struct Sha256 {
    state: [u32; 8],
    buffer: [u8; 64],
    buffered: usize,
    bytes: u64,
}

impl Sha256 {
    fn new() -> Self {
        Self {
            state: [
                0x6a09_e667,
                0xbb67_ae85,
                0x3c6e_f372,
                0xa54f_f53a,
                0x510e_527f,
                0x9b05_688c,
                0x1f83_d9ab,
                0x5be0_cd19,
            ],
            buffer: [0; 64],
            buffered: 0,
            bytes: 0,
        }
    }

    fn update(&mut self, mut input: &[u8]) {
        self.bytes = self.bytes.wrapping_add(input.len() as u64);
        if self.buffered > 0 {
            let count = input.len().min(64 - self.buffered);
            self.buffer[self.buffered..self.buffered + count].copy_from_slice(&input[..count]);
            self.buffered += count;
            input = &input[count..];
            if self.buffered == 64 {
                let block = self.buffer;
                self.transform(&block);
                self.buffered = 0;
            }
        }
        while input.len() >= 64 {
            let block: &[u8; 64] = input[..64]
                .try_into()
                .expect("slice has exact block length");
            self.transform(block);
            input = &input[64..];
        }
        self.buffer[..input.len()].copy_from_slice(input);
        self.buffered = input.len();
    }

    fn finalize(mut self) -> [u8; 32] {
        let bit_length = self.bytes.wrapping_mul(8);
        self.buffer[self.buffered] = 0x80;
        self.buffered += 1;
        if self.buffered > 56 {
            self.buffer[self.buffered..].fill(0);
            let block = self.buffer;
            self.transform(&block);
            self.buffer = [0; 64];
            self.buffered = 0;
        }
        self.buffer[self.buffered..56].fill(0);
        self.buffer[56..].copy_from_slice(&bit_length.to_be_bytes());
        let block = self.buffer;
        self.transform(&block);
        let mut output = [0_u8; 32];
        for (chunk, word) in output.chunks_exact_mut(4).zip(self.state) {
            chunk.copy_from_slice(&word.to_be_bytes());
        }
        output
    }

    fn transform(&mut self, block: &[u8; 64]) {
        const K: [u32; 64] = [
            0x428a_2f98,
            0x7137_4491,
            0xb5c0_fbcf,
            0xe9b5_dba5,
            0x3956_c25b,
            0x59f1_11f1,
            0x923f_82a4,
            0xab1c_5ed5,
            0xd807_aa98,
            0x1283_5b01,
            0x2431_85be,
            0x550c_7dc3,
            0x72be_5d74,
            0x80de_b1fe,
            0x9bdc_06a7,
            0xc19b_f174,
            0xe49b_69c1,
            0xefbe_4786,
            0x0fc1_9dc6,
            0x240c_a1cc,
            0x2de9_2c6f,
            0x4a74_84aa,
            0x5cb0_a9dc,
            0x76f9_88da,
            0x983e_5152,
            0xa831_c66d,
            0xb003_27c8,
            0xbf59_7fc7,
            0xc6e0_0bf3,
            0xd5a7_9147,
            0x06ca_6351,
            0x1429_2967,
            0x27b7_0a85,
            0x2e1b_2138,
            0x4d2c_6dfc,
            0x5338_0d13,
            0x650a_7354,
            0x766a_0abb,
            0x81c2_c92e,
            0x9272_2c85,
            0xa2bf_e8a1,
            0xa81a_664b,
            0xc24b_8b70,
            0xc76c_51a3,
            0xd192_e819,
            0xd699_0624,
            0xf40e_3585,
            0x106a_a070,
            0x19a4_c116,
            0x1e37_6c08,
            0x2748_774c,
            0x34b0_bcb5,
            0x391c_0cb3,
            0x4ed8_aa4a,
            0x5b9c_ca4f,
            0x682e_6ff3,
            0x748f_82ee,
            0x78a5_636f,
            0x84c8_7814,
            0x8cc7_0208,
            0x90be_fffa,
            0xa450_6ceb,
            0xbef9_a3f7,
            0xc671_78f2,
        ];
        let mut words = [0_u32; 64];
        for (index, chunk) in block.chunks_exact(4).enumerate() {
            words[index] = u32::from_be_bytes(chunk.try_into().expect("chunk has four bytes"));
        }
        for index in 16..64 {
            let s0 = words[index - 15].rotate_right(7)
                ^ words[index - 15].rotate_right(18)
                ^ (words[index - 15] >> 3);
            let s1 = words[index - 2].rotate_right(17)
                ^ words[index - 2].rotate_right(19)
                ^ (words[index - 2] >> 10);
            words[index] = words[index - 16]
                .wrapping_add(s0)
                .wrapping_add(words[index - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = self.state;
        for index in 0..64 {
            let sum1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let choice = (e & f) ^ ((!e) & g);
            let temp1 = h
                .wrapping_add(sum1)
                .wrapping_add(choice)
                .wrapping_add(K[index])
                .wrapping_add(words[index]);
            let sum0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let majority = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = sum0.wrapping_add(majority);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }
        self.state[0] = self.state[0].wrapping_add(a);
        self.state[1] = self.state[1].wrapping_add(b);
        self.state[2] = self.state[2].wrapping_add(c);
        self.state[3] = self.state[3].wrapping_add(d);
        self.state[4] = self.state[4].wrapping_add(e);
        self.state[5] = self.state[5].wrapping_add(f);
        self.state[6] = self.state[6].wrapping_add(g);
        self.state[7] = self.state[7].wrapping_add(h);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: [u8; 32]) -> String {
        let mut output = String::with_capacity(64);
        for byte in bytes {
            write!(output, "{byte:02x}").unwrap();
        }
        output
    }

    #[test]
    fn sha256_known_vectors() {
        let mut empty = Sha256::new();
        empty.update(b"");
        assert_eq!(
            hex(empty.finalize()),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        let mut abc = Sha256::new();
        abc.update(b"abc");
        assert_eq!(
            hex(abc.finalize()),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn deterministic_pattern_hash_matches_reference() {
        assert_eq!(
            payload_etag(64),
            "\"sha256-fdeab9acf3710362bd2658cdc9a29e8f9c757fcf9811603a8c447cd1d9151108\""
        );
    }

    #[test]
    fn parses_bounded_ranges() {
        assert_eq!(
            parse_byte_range("bytes=10-19", 100),
            Some(ByteRange { start: 10, end: 19 })
        );
        assert_eq!(
            parse_byte_range("bytes=90-", 100),
            Some(ByteRange { start: 90, end: 99 })
        );
        assert_eq!(
            parse_byte_range("bytes=-10", 100),
            Some(ByteRange { start: 90, end: 99 })
        );
        assert_eq!(parse_byte_range("bytes=100-101", 100), None);
        assert_eq!(parse_byte_range("bytes=20-10", 100), None);
        assert_eq!(parse_byte_range("bytes=1-2,4-5", 100), None);
    }

    #[test]
    fn locates_complete_request_headers() {
        assert_eq!(
            find_header_end(b"GET / HTTP/1.1\r\nHost: test\r\n\r\n"),
            Some(26)
        );
        assert_eq!(find_header_end(b"GET / HTTP/1.1\r\n"), None);
    }
}
