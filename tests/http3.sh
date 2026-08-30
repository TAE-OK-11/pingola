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
  --bin pingora --example http3_probe --example pq_tls_probe
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

pq_curve=$("${ROOT}/target/debug/examples/pq_tls_probe" \
  127.0.0.1:18444 app.test)
[[ "${pq_curve}" == "X25519MLKEM768" ]]

# Browsers normally discover HTTP/3 from an initial H1 or H2 TLS
# response. Direct redirects must advertise the configured UDP port
# even though they bypass Pingora's upstream response filter.
for protocol in --http1.1 --http2; do
  headers=$(curl --noproxy '*' -gksSI "${protocol}"     --resolve app.test:18444:127.0.0.1 https://app.test:18444/)
  grep -q '^HTTP/' <<<"${headers}"
  grep -qi '^alt-svc: h3=":18443"; ma=86400' <<<"${headers}"
  grep -qi '^location: https://app.test/app/' <<<"${headers}"
done

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
jq -e '.headers.connection == null' <<<"${app_response}" >/dev/null

static_response=$("${ROOT}/target/debug/examples/http3_probe" \
  127.0.0.1:18443 static.test /)
grep -q 'pingora-static-response' <<<"${static_response}"

location=$(curl --noproxy '*' -sSI -H 'host: app.test' \
  http://127.0.0.1:18081/headers | awk -F': ' \
  'tolower($1) == "location" {gsub("\r", "", $2); print $2}')
[[ "${location}" == "https://app.test/headers" ]]

grep -q 'HTTP/3 frontend started:.*internal=direct-gateway' "${GATEWAY_LOG}"
grep -q 'HTTP/3 frontend started:.*hybrid_pq=X25519MLKEM768:X25519:P-256.*stateless_retry=true.*max_amplification=3' "${GATEWAY_LOG}"
grep -q 'http3_udp=\["127.0.0.1:18443"\]' "${GATEWAY_LOG}"
grep -q 'downstream_h3=direct upstream_h3=direct' "${GATEWAY_LOG}"

echo "HTTP/3 hybrid PQ TLS, stateless Retry, anti-DDoS admission, direct QUIC proxy, Alt-Svc, forwarding, and isolation tests passed"
