#!/usr/bin/env python3
import json
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    server_version = "backend-secret"
    sys_version = ""

    def _respond(self, include_body=True):
        length = int(self.headers.get("content-length", "0"))
        body = self.rfile.read(length) if length else b""
        payload = json.dumps(
            {
                "method": self.command,
                "path": self.path,
                "body_length": len(body),
                "headers": {key.lower(): value for key, value in self.headers.items()},
            },
            separators=(",", ":"),
        ).encode()
        self.send_response(200)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(payload)))
        for index in range(20):
            self.send_header(f"x-spill-response-{index}", f"value-{index}")
        self.end_headers()
        if include_body:
            self.wfile.write(payload)

    def do_GET(self):
        self._respond()

    def do_HEAD(self):
        self._respond(include_body=False)

    def do_POST(self):
        self._respond()

    def log_message(self, _format, *_args):
        pass


if __name__ == "__main__":
    ThreadingHTTPServer(("127.0.0.1", 19090), Handler).serve_forever()
