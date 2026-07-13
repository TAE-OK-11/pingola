#!/usr/bin/env python3
"""Deterministic HTTP/1.1 backend for HTTP/2 proxy correctness tests."""

import hashlib
import json
import socket
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


PAYLOADS = {
    64: bytes((index * 17 + 11) % 256 for index in range(64)),
    4096: bytes((index * 17 + 11) % 256 for index in range(4096)),
}


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    server_version = "h2-matrix-backend"
    sys_version = ""

    def _fixed(self, size: int, include_body: bool = True, close: bool = False) -> None:
        payload = PAYLOADS[size]
        self.send_response(200)
        self.send_header("Content-Type", "application/octet-stream")
        self.send_header("Content-Length", str(len(payload)))
        self.send_header("X-Body-SHA256", hashlib.sha256(payload).hexdigest())
        self.send_header("Connection", "close" if close else "keep-alive")
        self.end_headers()
        if include_body:
            self.wfile.write(payload)
            self.wfile.flush()
        if close:
            self.close_connection = True

    def _chunked(self, include_trailer: bool = False) -> None:
        payload = PAYLOADS[4096]
        chunks = (payload[:31], payload[31:1055], payload[1055:])
        self.send_response(200)
        self.send_header("Content-Type", "application/octet-stream")
        self.send_header("Transfer-Encoding", "chunked")
        self.send_header("Connection", "keep-alive")
        self.send_header("X-Body-SHA256", hashlib.sha256(payload).hexdigest())
        if include_trailer:
            self.send_header("Trailer", "X-Checksum-Trailer")
        self.end_headers()
        for chunk in chunks:
            self.wfile.write(f"{len(chunk):x}\r\n".encode("ascii"))
            self.wfile.write(chunk)
            self.wfile.write(b"\r\n")
        self.wfile.write(b"0\r\n")
        if include_trailer:
            self.wfile.write(b"X-Checksum-Trailer: complete\r\n")
        self.wfile.write(b"\r\n")
        self.wfile.flush()

    def _early_eof(self) -> None:
        payload = PAYLOADS[64]
        self.send_response(200)
        self.send_header("Content-Type", "application/octet-stream")
        self.send_header("Content-Length", str(len(payload)))
        self.send_header("Connection", "close")
        self.end_headers()
        self.wfile.write(payload[:32])
        self.wfile.flush()
        self.close_connection = True
        try:
            self.connection.shutdown(socket.SHUT_RDWR)
        except OSError:
            pass
        self.connection.close()

    def do_GET(self) -> None:
        if self.path == "/fixed/64" or self.path == "/keepalive/64":
            self._fixed(64)
        elif self.path == "/fixed/4096":
            self._fixed(4096)
        elif self.path == "/close/64":
            self._fixed(64, close=True)
        elif self.path == "/chunked/4096":
            self._chunked()
        elif self.path == "/trailer/4096":
            self._chunked(include_trailer=True)
        elif self.path == "/empty/204":
            self.send_response(204)
            self.send_header("Connection", "keep-alive")
            self.end_headers()
        elif self.path == "/early-eof/64":
            self._early_eof()
        elif self.path == "/metadata":
            payload = json.dumps(
                {
                    str(size): hashlib.sha256(body).hexdigest()
                    for size, body in PAYLOADS.items()
                },
                separators=(",", ":"),
            ).encode("ascii")
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)
        else:
            self.send_error(404)

    def do_HEAD(self) -> None:
        if self.path == "/fixed/64":
            self._fixed(64, include_body=False)
        else:
            self.send_error(404)

    def log_message(self, message: str, *args: object) -> None:
        print(f"{self.client_address[0]} {message % args}", file=sys.stderr, flush=True)


if __name__ == "__main__":
    ThreadingHTTPServer(("127.0.0.1", 19091), Handler).serve_forever()
