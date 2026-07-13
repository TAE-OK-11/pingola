#!/usr/bin/env python3
import hashlib
import json
import os
import socket
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import urlsplit


PATTERN = bytes((index * 29 + 7) % 256 for index in range(64 * 1024))
DISCONNECT_MARKER = os.environ["DISCONNECT_MARKER"]


def chunks(offset: int, length: int, chunk_size: int = 64 * 1024):
    remaining = length
    position = offset
    while remaining:
        size = min(remaining, chunk_size)
        start = position % len(PATTERN)
        block = PATTERN[start : start + size]
        if len(block) < size:
            block += PATTERN[: size - len(block)]
        yield block
        position += size
        remaining -= size


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    server_version = "service-matrix"
    sys_version = ""

    def _stream(self, size: int, mode: str, include_body: bool = True) -> None:
        start = 0
        end = size - 1
        status = 200
        range_header = self.headers.get("range")
        if range_header and range_header.startswith("bytes="):
            first, _, last = range_header[6:].partition("-")
            start = int(first)
            end = min(int(last) if last else size - 1, size - 1)
            status = 206
        length = max(0, end - start + 1)
        etag = f'"synthetic-{size}"'
        if self.headers.get("if-none-match") == etag and status == 200:
            self.send_response(304)
            self.send_header("ETag", etag)
            self.end_headers()
            return

        self.send_response(status)
        self.send_header("Content-Type", "audio/flac")
        self.send_header("Accept-Ranges", "bytes")
        self.send_header("ETag", etag)
        self.send_header("Last-Modified", "Mon, 13 Jul 2026 00:00:00 GMT")
        self.send_header("Cache-Control", "private, max-age=0")
        if status == 206:
            self.send_header("Content-Range", f"bytes {start}-{end}/{size}")
        if mode == "chunked":
            self.send_header("Transfer-Encoding", "chunked")
        else:
            self.send_header("Content-Length", str(length))
        if mode == "reset":
            self.send_header("Connection", "close")
        self.end_headers()
        if not include_body:
            return

        sent = 0
        try:
            for block in chunks(start, length):
                if mode == "reset" and sent >= length // 2:
                    self.close_connection = True
                    self.connection.shutdown(socket.SHUT_RDWR)
                    self.connection.close()
                    return
                if mode == "chunked":
                    self.wfile.write(f"{len(block):x}\r\n".encode("ascii"))
                    self.wfile.write(block)
                    self.wfile.write(b"\r\n")
                else:
                    self.wfile.write(block)
                self.wfile.flush()
                sent += len(block)
                if mode == "slow":
                    time.sleep(0.005)
                elif mode == "pause" and sent >= length // 2:
                    mode = "fixed"
                    time.sleep(0.5)
            if mode == "chunked":
                self.wfile.write(b"0\r\nX-Stream-Trailer: complete\r\n\r\n")
                self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError, OSError):
            with open(DISCONNECT_MARKER, "w", encoding="ascii") as output:
                output.write("upstream-cancelled\n")

    def _json_headers(self) -> None:
        payload = json.dumps(
            {
                "method": self.command,
                "accept_encoding": self.headers.get("accept-encoding"),
                "range": self.headers.get("range"),
                "upgrade": self.headers.get("upgrade"),
                "connection": self.headers.get("connection"),
            },
            separators=(",", ":"),
        ).encode("ascii")
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def do_GET(self) -> None:
        path = urlsplit(self.path).path
        if path in ("/api", "/lyrics", "/cover", "/headers", "/stream/headers"):
            self._json_headers()
            return
        if path == "/dns-query":
            payload = b"\x00\x01synthetic-dns-response"
            self.send_response(200)
            self.send_header("Content-Type", "application/dns-message")
            self.send_header("Content-Length", str(len(payload)))
            self.send_header("Cache-Control", "public, max-age=3600")
            self.end_headers()
            self.wfile.write(payload)
            return
        parts = path.strip("/").split("/")
        if len(parts) == 2 and parts[0] in {
            "stream",
            "stream-slow",
            "stream-pause",
            "stream-reset",
            "stream-chunked",
            "attachment",
            "replication",
        }:
            mode = {
                "stream-slow": "slow",
                "stream-pause": "pause",
                "stream-reset": "reset",
                "stream-chunked": "chunked",
                "replication": "chunked",
            }.get(parts[0], "fixed")
            self._stream(int(parts[1]), mode)
            return
        self.send_error(404)

    def do_HEAD(self) -> None:
        path = urlsplit(self.path).path
        parts = path.strip("/").split("/")
        if len(parts) == 2 and parts[0] == "stream":
            self._stream(int(parts[1]), "fixed", include_body=False)
        else:
            self.send_error(404)

    def do_POST(self) -> None:
        path = urlsplit(self.path).path
        length = int(self.headers.get("content-length", "0"))
        body = self.rfile.read(length) if length else b""
        if path == "/dns-query":
            payload = body or b"\x00\x01empty-query"
            self.send_response(200)
            self.send_header("Content-Type", "application/dns-message")
            self.send_header("Content-Length", str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)
            return
        payload = hashlib.sha256(body).hexdigest().encode("ascii")
        self.send_response(200)
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    do_PUT = do_POST

    def log_message(self, _format: str, *_args: object) -> None:
        pass


ThreadingHTTPServer(("127.0.0.1", 19996), Handler).serve_forever()
