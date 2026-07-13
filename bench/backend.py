#!/usr/bin/env python3
"""Deterministic localhost backend for proxy correctness and benchmarks."""

import argparse
import hashlib
import socket
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


PATTERN = bytes(range(256))


def payload(size: int) -> bytes:
    return (PATTERN * ((size + len(PATTERN) - 1) // len(PATTERN)))[:size]


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, fmt: str, *args: object) -> None:
        return

    def do_HEAD(self) -> None:
        self._serve(send_body=False)

    def do_GET(self) -> None:
        self._serve(send_body=True)

    def do_POST(self) -> None:
        length = int(self.headers.get("content-length", "0"))
        body = self.rfile.read(length)
        self.send_response(200)
        self.send_header("Content-Type", "application/dns-message")
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Cache-Control", "max-age=3600")
        self.end_headers()
        self.wfile.write(body)

    def _serve(self, send_body: bool) -> None:
        path = self.path.split("?", 1)[0]
        if path == "/status/204":
            self.send_response(204)
            self.end_headers()
            return
        if path == "/reset":
            self.connection.shutdown(socket.SHUT_RDWR)
            self.connection.close()
            return
        if path.startswith("/pause/"):
            size = int(path.rsplit("/", 1)[1])
            data = payload(size)
            self.send_response(200)
            self.send_header("Content-Type", "application/octet-stream")
            self.send_header("Transfer-Encoding", "chunked")
            self.end_headers()
            if send_body:
                midpoint = len(data) // 2
                self._chunk(data[:midpoint])
                time.sleep(1.0)
                self._chunk(data[midpoint:])
                self.wfile.write(b"0\r\n\r\n")
            return
        if path.startswith("/stream/"):
            size = int(path.rsplit("/", 1)[1])
            self.send_response(200)
            self.send_header("Content-Type", "application/octet-stream")
            self.send_header("Transfer-Encoding", "chunked")
            self.end_headers()
            if send_body:
                remaining = size
                while remaining:
                    part = payload(min(16 * 1024, remaining))
                    self._chunk(part)
                    remaining -= len(part)
                self.wfile.write(b"0\r\n\r\n")
            return
        if not path.startswith("/bytes/"):
            self.send_error(404)
            return
        size = int(path.rsplit("/", 1)[1])
        data = payload(size)
        start = 0
        end = size - 1
        status = 200
        requested_range = self.headers.get("range")
        if requested_range and requested_range.startswith("bytes="):
            first, last = requested_range[6:].split("-", 1)
            start = int(first or "0")
            end = int(last or str(end))
            end = min(end, size - 1)
            status = 206
            data = data[start : end + 1]
        self.send_response(status)
        self.send_header("Content-Type", "application/octet-stream")
        self.send_header("Content-Length", str(len(data)))
        self.send_header("Accept-Ranges", "bytes")
        self.send_header("ETag", f'"sha256-{hashlib.sha256(payload(size)).hexdigest()}"')
        if status == 206:
            self.send_header("Content-Range", f"bytes {start}-{end}/{size}")
        self.end_headers()
        if send_body:
            self.wfile.write(data)

    def _chunk(self, data: bytes) -> None:
        if data:
            self.wfile.write(f"{len(data):x}\r\n".encode() + data + b"\r\n")
            self.wfile.flush()


class Server(ThreadingHTTPServer):
    daemon_threads = True
    request_queue_size = 1024

    def get_request(self) -> tuple[socket.socket, tuple[str, int]]:
        connection, address = super().get_request()
        connection.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        return connection, address


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--listen", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=18700)
    args = parser.parse_args()
    Server((args.listen, args.port), Handler).serve_forever()


if __name__ == "__main__":
    main()
