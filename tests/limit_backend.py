#!/usr/bin/env python3
import os
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


MARKER = os.environ["LIMIT_MARKER"]


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def do_GET(self) -> None:
        if self.path.startswith("/stream"):
            with open(MARKER, "w", encoding="ascii") as output:
                output.write("active\n")
            time.sleep(2)
        self.send_response(200)
        self.send_header("Content-Length", "0")
        self.end_headers()

    def log_message(self, _format: str, *_args: object) -> None:
        pass


ThreadingHTTPServer(("127.0.0.1", 19997), Handler).serve_forever()
