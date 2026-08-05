#!/usr/bin/env bash
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
RUNTIME=/tmp/pingora-http3
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
trap cleanup EXIT INT TERM

rm -rf "${RUNTIME}"
install -d -m 0755 "${RUNTIME}/www"
cp "${ROOT}/tests/fixtures/www/index.html" "${RUNTIME}/www/index.html"
openssl req -x509 -newkey rsa:2048 -nodes -days 1 \
  -subj "/CN=app.test" \
  -addext "subjectAltName=DNS:app.test,DNS:static.test" \
  -keyout "${RUNTIME}/key.pem" -out "${RUNTIME}/cert.pem" >/dev/null 2>&1
chmod 0600 "${RUNTIME}/key.pem"
chmod 0644 "${RUNTIME}/cert.pem"

cargo build --manifest-path "${ROOT}/Cargo.toml" --locked \
  --bin pingora --example http3_probe
python3 "${ROOT}/tests/backend.py" >"${BACKEND_LOG}" 2>&1 &
BACKEND_PID=$!
RUST_LOG=info "${ROOT}/target/debug/pingora" \
  --config "${ROOT}/tests/fixtures/http3.yaml" >"${GATEWAY_LOG}" 2>&1 &
GATEWAY_PID=$!

for _ in {1..100}; do
  if curl --noproxy '*' -fsS -H 'host: health.invalid' \
    http://127.0.0.1:18081/pingora-ready -o /dev/null 2>/dev/null; then
    break
  fi
  if ! kill -0 "${GATEWAY_PID}" 2>/dev/null; then
    cat "${GATEWAY_LOG}" >&2
    exit 1
  fi
  sleep 0.1
done
kill -0 "${GATEWAY_PID}"

# The private handoff listener must accept HTTP/2 prior knowledge. This request
# intentionally lacks the random internal token, so a successful h2c exchange
# returns the normal HTTPS redirect rather than entering the trusted H3 path.
h2c_probe=$(nghttp --no-dep -nv -H ':authority: app.test' \
  http://127.0.0.1:18080/headers 2>&1)
grep -q ':status: 308' <<<"${h2c_probe}"

app_response=$("${ROOT}/target/debug/examples/http3_probe" \
  127.0.0.1:18443 app.test /headers)
jq -e '.method == "GET"' <<<"${app_response}" >/dev/null
jq -e '.path == "/headers"' <<<"${app_response}" >/dev/null
jq -e '.headers["x-forwarded-for"] == "127.0.0.1"' \
  <<<"${app_response}" >/dev/null
jq -e '.headers["x-forwarded-proto"] == "https"' \
  <<<"${app_response}" >/dev/null
jq -e '.headers["x-forwarded-port"] == "18443"' \
  <<<"${app_response}" >/dev/null
jq -e '.headers["x-jbs-http3-internal"] == null' \
  <<<"${app_response}" >/dev/null
jq -e '.headers["x-jbs-http3-port"] == null' \
  <<<"${app_response}" >/dev/null
jq -e '.headers.connection == null' <<<"${app_response}" >/dev/null

static_response=$("${ROOT}/target/debug/examples/http3_probe" \
  127.0.0.1:18443 static.test /)
grep -q 'pingora-static-response' <<<"${static_response}"

# The same authority over plaintext TCP remains redirected while the trusted
# HTTP/3 loopback handoff is treated as an authenticated HTTPS transport.
location=$(curl --noproxy '*' -sSI -H 'host: app.test' \
  http://127.0.0.1:18081/headers | awk -F': ' \
  'tolower($1) == "location" {gsub("\r", "", $2); print $2}')
[[ "${location}" == "https://app.test/headers" ]]

# A loopback caller cannot forge the private H3 handoff with the old static
# marker. The request must still be treated as plaintext and redirected.
spoof_location=$(curl --noproxy '*' -sSI -H 'host: app.test' \
  -H 'x-jbs-http3-internal: 1' -H 'x-jbs-http3-port: 18443' \
  http://127.0.0.1:18080/headers | awk -F': ' \
  'tolower($1) == "location" {gsub("\r", "", $2); print $2}')
[[ "${spoof_location}" == "https://app.test/headers" ]]

grep -q 'HTTP/3 frontend started:.*internal=h2c://' "${GATEWAY_LOG}"
grep -q 'http3_udp=\["127.0.0.1:18443"\]' "${GATEWAY_LOG}"

echo "HTTP/3 QUIC proxy, h2c internal multiplexing, static response, Alt-Svc, forwarding, and private-header isolation tests passed"
