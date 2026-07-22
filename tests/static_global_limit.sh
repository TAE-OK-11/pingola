#!/usr/bin/env bash
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
RUNTIME=/tmp/pingora-static-global-limit
BIN=${ROOT}/target/debug/pingora
GATEWAY_PID=
HOLDER_PID=

cleanup() {
  for pid in "${HOLDER_PID}" "${GATEWAY_PID}"; do
    if [[ -n "${pid}" ]]; then
      kill "${pid}" 2>/dev/null || true
      wait "${pid}" 2>/dev/null || true
    fi
  done
}
trap cleanup EXIT INT TERM

rm -rf "${RUNTIME}"
mkdir -p "${RUNTIME}/www"
truncate -s 67108864 "${RUNTIME}/www/large.bin"
cat >"${RUNTIME}/pingora.yaml" <<EOF
server:
  http_listen: ["127.0.0.1:80"]
  https_listen: []
  global_active_requests: 1
  graceful_shutdown_timeout_seconds: 1
  health_socket: ${RUNTIME}/health.sock
trusted_proxies: ["127.0.0.0/8"]
upstreams: {}
hosts:
  static:
    domains: ["static-limit.test"]
    handler: static
    static_root: ${RUNTIME}/www
EOF

RUST_LOG=warn "${BIN}" --config "${RUNTIME}/pingora.yaml" \
  >"${RUNTIME}/gateway.stdout" 2>"${RUNTIME}/gateway.stderr" &
GATEWAY_PID=$!
for _ in {1..100}; do
  if "${BIN}" --config "${RUNTIME}/pingora.yaml" --healthcheck >/dev/null 2>&1; then
    break
  fi
  sleep 0.1
done
kill -0 "${GATEWAY_PID}"

RUNTIME="${RUNTIME}" python3 - <<'PY' &
import os
import socket
import time

runtime = os.environ["RUNTIME"]
connection = socket.create_connection(("127.0.0.1", 80), timeout=5)
connection.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 4096)
connection.sendall(
    b"GET /large.bin HTTP/1.1\r\n"
    b"Host: static-limit.test\r\n"
    b"Connection: close\r\n\r\n"
)
headers = b""
while b"\r\n\r\n" not in headers:
    chunk = connection.recv(4096)
    if not chunk:
        raise SystemExit("large static response closed before headers")
    headers += chunk
with open(os.path.join(runtime, "holder-active"), "w", encoding="ascii") as marker:
    marker.write("active\n")
time.sleep(3)
connection.close()
PY
HOLDER_PID=$!

for _ in {1..100}; do
  [[ -f "${RUNTIME}/holder-active" ]] && break
  sleep 0.02
done
[[ -f "${RUNTIME}/holder-active" ]]
sleep 0.1

status=$(curl --noproxy '*' -sS -I -o /dev/null -w '%{http_code}' \
  -H 'host: static-limit.test' http://127.0.0.1:80/large.bin)
[[ "${status}" == 429 ]]

wait "${HOLDER_PID}"
HOLDER_PID=
for _ in {1..100}; do
  status=$(curl --noproxy '*' -sS -I -o /dev/null -w '%{http_code}' \
    -H 'host: static-limit.test' http://127.0.0.1:80/large.bin)
  [[ "${status}" == 200 ]] && break
  sleep 0.02
done
[[ "${status}" == 200 ]]

echo "static global active-request limit passed"
