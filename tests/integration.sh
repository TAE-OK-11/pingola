#!/usr/bin/env bash
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
RUNTIME=/tmp/pingora-integration
GATEWAY_LOG=${RUNTIME}/gateway.log
BACKEND_LOG=${RUNTIME}/backend.log
GATEWAY_PID=
BACKEND_PID=

cleanup() {
  if [[ -n "${GATEWAY_PID}" ]]; then
    kill "${GATEWAY_PID}" 2>/dev/null || true
    wait "${GATEWAY_PID}" 2>/dev/null || true
  fi
  if [[ -n "${BACKEND_PID}" ]]; then
    kill "${BACKEND_PID}" 2>/dev/null || true
    wait "${BACKEND_PID}" 2>/dev/null || true
  fi
}
trap cleanup EXIT

rm -rf "${RUNTIME}"
mkdir -p "${RUNTIME}/www"
cp "${ROOT}/tests/fixtures/www/index.html" "${RUNTIME}/www/index.html"
truncate -s 8388609 "${RUNTIME}/www/large.bin"
openssl req -x509 -newkey rsa:2048 -nodes -days 1 \
  -subj "/CN=static.test" \
  -addext "subjectAltName=DNS:static.test,DNS:app.test,DNS:vault.test" \
  -keyout "${RUNTIME}/key.pem" -out "${RUNTIME}/cert.pem" >/dev/null 2>&1

cargo build --manifest-path "${ROOT}/Cargo.toml"
python3 "${ROOT}/tests/backend.py" >"${BACKEND_LOG}" 2>&1 &
BACKEND_PID=$!
RUST_LOG=info "${ROOT}/target/debug/pingora" \
  --config "${ROOT}/tests/fixtures/integration.yaml" >"${GATEWAY_LOG}" 2>&1 &
GATEWAY_PID=$!

for _ in {1..50}; do
  if curl --noproxy '*' -fsS -H 'host: health.invalid' \
    http://127.0.0.1:80/pingora-health -o /dev/null 2>/dev/null; then
    break
  fi
  sleep 0.1
done
kill -0 "${GATEWAY_PID}"
"${ROOT}/target/debug/pingora" --config "${ROOT}/tests/fixtures/integration.yaml" --healthcheck

status=$(curl --noproxy '*' -sS -o /dev/null -w '%{http_code}' \
  -H 'host: unknown.test' http://127.0.0.1:80/)
[[ "${status}" == "421" ]]

location=$(curl --noproxy '*' -sSI -H 'host: app.test' \
  'http://127.0.0.1:80/hello?x=1' | awk -F': ' \
  'tolower($1) == "location" {gsub("\r", "", $2); print $2}')
[[ "${location}" == "https://app.test/hello?x=1" ]]

static_body=$(curl --noproxy '*' --compressed -fsS -H 'host: static.test' \
  -H 'accept-encoding: gzip' http://127.0.0.1:80/)
grep -q 'pingora-static-response' <<<"${static_body}"

curl --noproxy '*' -sSI -H 'host: static.test' -H 'accept-encoding: zstd' \
  http://127.0.0.1:80/ | grep -qi '^content-encoding: zstd'

curl --noproxy '*' -fsS -H 'host: static.test' \
  http://127.0.0.1:80/large.bin -o "${RUNTIME}/large-response.bin"
[[ "$(stat -c '%s' "${RUNTIME}/large-response.bin")" == "8388609" ]]

http_version=$(curl --noproxy '*' -ksS --http2 \
  --resolve static.test:443:127.0.0.1 -o /dev/null -w '%{http_version}' \
  https://static.test:443/)
[[ "${http_version}" == "2" ]]

curl --noproxy '*' -ksSI --http2 --resolve static.test:443:127.0.0.1 \
  https://static.test:443/ | \
  grep -qi '^strict-transport-security: max-age=63072000; includeSubDomains; preload'

openssl s_client -connect 127.0.0.1:443 -servername static.test \
  -alpn h2 -tls1_3 </dev/null 2>&1 | tr -d '\000' >"${RUNTIME}/tls13.log"
grep -q 'New, TLSv1.3' "${RUNTIME}/tls13.log"
grep -q 'ALPN protocol: h2' "${RUNTIME}/tls13.log"
if openssl s_client -connect 127.0.0.1:443 -servername static.test \
  -tls1_2 </dev/null 2>&1 | grep -q 'New, TLSv1.2'; then
  echo 'TLS 1.2 was unexpectedly accepted' >&2
  exit 1
fi

proxy_response=$(curl --noproxy '*' -ksS --http2 \
  --resolve app.test:443:127.0.0.1 \
  -H 'x-forwarded-for: 198.51.100.50, 10.0.0.2' \
  https://app.test:443/hello)
jq -e '.headers["x-forwarded-for"] == "198.51.100.50"' \
  <<<"${proxy_response}" >/dev/null
jq -e '.headers["x-forwarded-proto"] == "https"' \
  <<<"${proxy_response}" >/dev/null
jq -e '.headers["x-forwarded-port"] == "443"' \
  <<<"${proxy_response}" >/dev/null

hop_response=$(curl --noproxy '*' -ksS --http1.1 \
  --resolve app.test:443:127.0.0.1 \
  -H 'connection: keep-alive, x-private' \
  -H 'x-private: must-not-reach-upstream' \
  -H 'proxy-authorization: Basic must-not-reach-upstream' \
  https://app.test:443/headers)
jq -e '.headers["x-private"] == null' <<<"${hop_response}" >/dev/null
jq -e '.headers["proxy-authorization"] == null' <<<"${hop_response}" >/dev/null
jq -e '.headers.connection == null' <<<"${hop_response}" >/dev/null

# The optimized HTTP/1 upstream request clone intentionally drops only the
# header-spelling side map. Header values and the raw percent-encoded request
# target must still reach the upstream byte-for-byte.
python3 - <<'PY'
import json
import socket
import ssl

raw_target = b"/raw-%FF?x=1"
context = ssl.create_default_context()
context.check_hostname = False
context.verify_mode = ssl.CERT_NONE
context.set_alpn_protocols(["http/1.1"])
raw = socket.create_connection(("127.0.0.1", 443), timeout=5)
connection = context.wrap_socket(raw, server_hostname="app.test")
connection.settimeout(5)
spill_headers = b"".join(
    f"X-Spill-{index}: value-{index}\r\n".encode() for index in range(20)
)
request = (
    b"GET "
    + raw_target
    + b" HTTP/1.1\r\nhOsT: app.test\r\nX-MiXeD: preserved\r\n"
    + spill_headers
    + b"Connection: close\r\n\r\n"
)
# Exercise the H1 partial-parse path as well as the inline-to-heap header
# offset spill. The first write deliberately ends in the middle of a field.
split = request.index(b"X-Spill-10") + len(b"X-Spi")
connection.sendall(request[:split])
connection.sendall(request[split:])

buffer = b""
while b"\r\n\r\n" not in buffer:
    chunk = connection.recv(65536)
    if not chunk:
        raise SystemExit("raw-path response closed before headers")
    buffer += chunk
header, body = buffer.split(b"\r\n\r\n", 1)
lines = header.split(b"\r\n")
if b" 200 " not in lines[0]:
    raise SystemExit(f"raw-path request failed: {lines[0]!r}")
headers = {}
for line in lines[1:]:
    name, value = line.split(b":", 1)
    headers[name.lower()] = value.strip()
if headers.get(b"x-spill-response-0") != b"value-0":
    raise SystemExit("first inline/spill response header was not preserved")
if headers.get(b"x-spill-response-19") != b"value-19":
    raise SystemExit("last spilled response header was not preserved")
length = int(headers[b"content-length"])
while len(body) < length:
    chunk = connection.recv(65536)
    if not chunk:
        raise SystemExit("raw-path response body was truncated")
    body += chunk
connection.close()

payload = json.loads(body[:length])
if payload["path"].encode("latin-1") != raw_target:
    raise SystemExit(f"raw path changed in proxy: {payload['path']!r}")
if payload["headers"].get("x-mixed") != "preserved":
    raise SystemExit("mixed-case request header value was not preserved")
if payload["headers"].get("x-spill-0") != "value-0":
    raise SystemExit("first inline/spill request header was not preserved")
if payload["headers"].get("x-spill-19") != "value-19":
    raise SystemExit("last spilled request header was not preserved")
PY

# The configured request keepalive limit must count down across reused HTTP/1.1
# sessions. Resetting it in request_filter makes the connection effectively
# unlimited and retains per-connection allocations indefinitely.
python3 - <<'PY'
import socket
import ssl

context = ssl.create_default_context()
context.check_hostname = False
context.verify_mode = ssl.CERT_NONE
context.set_alpn_protocols(["http/1.1"])
raw = socket.create_connection(("127.0.0.1", 443), timeout=5)
connection = context.wrap_socket(raw, server_hostname="vault.test")
connection.settimeout(5)
buffer = b""
completed = 0

expected = 37
for _ in range(expected + 1):
    try:
        connection.sendall(
            b"GET /hello HTTP/1.1\r\nHost: vault.test\r\nConnection: keep-alive\r\n\r\n"
        )
    except (BrokenPipeError, ssl.SSLError):
        break

    while b"\r\n\r\n" not in buffer:
        chunk = connection.recv(65536)
        if not chunk:
            break
        buffer += chunk
    if b"\r\n\r\n" not in buffer:
        break

    header, buffer = buffer.split(b"\r\n\r\n", 1)
    lines = header.split(b"\r\n")
    if b" 200 " not in lines[0]:
        raise SystemExit(f"unexpected keepalive response: {lines[0]!r}")
    headers = {}
    for line in lines[1:]:
        name, value = line.split(b":", 1)
        headers[name.lower()] = value.strip()
    length = int(headers[b"content-length"])
    while len(buffer) < length:
        chunk = connection.recv(65536)
        if not chunk:
            raise SystemExit("connection closed before response body completed")
        buffer += chunk
    buffer = buffer[length:]
    completed += 1

connection.close()
if completed != expected:
    raise SystemExit(
        f"downstream keepalive limit mismatch: completed={completed}, expected={expected}"
    )
PY

status=$(curl --noproxy '*' -ksS --http2 -o /dev/null -w '%{http_code}' \
  --resolve app.test:443:127.0.0.1 \
  --data 'this-body-is-over-sixteen-bytes' https://app.test:443/upload)
[[ "${status}" == "413" ]]

for _ in {1..4}; do
  status=$(curl --noproxy '*' -ksS --http2 -o /dev/null -w '%{http_code}' \
    --resolve vault.test:443:127.0.0.1 \
    https://vault.test:443/api/accounts/login)
  [[ "${status}" == "200" ]]
done
status=$(curl --noproxy '*' -ksS --http2 -o /dev/null -w '%{http_code}' \
  --resolve vault.test:443:127.0.0.1 \
  https://vault.test:443/api/accounts/login)
[[ "${status}" == "429" ]]

if curl --noproxy '*' -ksSI --http2 --resolve app.test:443:127.0.0.1 \
  https://app.test:443/hello | grep -qi '^server:'; then
  echo 'upstream Server header leaked' >&2
  exit 1
fi

status=$(curl --noproxy '*' -sS -o /dev/null -w '%{http_code}' \
  -H 'host: health.invalid' http://127.0.0.1:80/nginx-health)
[[ "${status}" == "404" ]]

echo 'Pingora integration checks passed'
