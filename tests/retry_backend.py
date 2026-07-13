#!/usr/bin/env python3
import os
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


COUNT_FILE = os.environ["RETRY_COUNT_FILE"]


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def _respond(self) -> None:
        length = int(self.headers.get("content-length", "0"))
        if length:
            self.rfile.read(length)
        with open(COUNT_FILE, "a", encoding="ascii") as output:
            output.write(f"{self.command} {self.path}\n")
        self.send_response(503)
        self.send_header("Content-Length", "0")
        self.end_headers()

    do_GET = _respond
    do_POST = _respond
    do_PUT = _respond

    def log_message(self, _format: str, *_args: object) -> None:
        pass


ThreadingHTTPServer(("127.0.0.1", 19998), Handler).serve_forever()
