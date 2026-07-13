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
    http://127.0.0.1:18080/pingora-health -o /dev/null 2>/dev/null; then
    break
  fi
  sleep 0.1
done
kill -0 "${GATEWAY_PID}"
"${ROOT}/target/debug/pingora" --config "${ROOT}/tests/fixtures/integration.yaml" --healthcheck

status=$(curl --noproxy '*' -sS -o /dev/null -w '%{http_code}' \
  -H 'host: unknown.test' http://127.0.0.1:18080/)
[[ "${status}" == "421" ]]

location=$(curl --noproxy '*' -sSI -H 'host: app.test' \
  'http://127.0.0.1:18080/hello?x=1' | awk -F': ' \
  'tolower($1) == "location" {gsub("\r", "", $2); print $2}')
[[ "${location}" == "https://app.test/hello?x=1" ]]

static_body=$(curl --noproxy '*' --compressed -fsS -H 'host: static.test' \
  -H 'accept-encoding: gzip' http://127.0.0.1:18080/)
grep -q 'pingora-static-response' <<<"${static_body}"

curl --noproxy '*' -sSI -H 'host: static.test' -H 'accept-encoding: zstd' \
  http://127.0.0.1:18080/ | grep -qi '^content-encoding: zstd'

curl --noproxy '*' -fsS -H 'host: static.test' \
  http://127.0.0.1:18080/large.bin -o "${RUNTIME}/large-response.bin"
[[ "$(stat -c '%s' "${RUNTIME}/large-response.bin")" == "8388609" ]]

http_version=$(curl --noproxy '*' -ksS --http2 \
  --resolve static.test:18443:127.0.0.1 -o /dev/null -w '%{http_version}' \
  https://static.test:18443/)
[[ "${http_version}" == "2" ]]

curl --noproxy '*' -ksSI --http2 --resolve static.test:18443:127.0.0.1 \
  https://static.test:18443/ | \
  grep -qi '^strict-transport-security: max-age=63072000; includeSubDomains; preload'

openssl s_client -connect 127.0.0.1:18443 -servername static.test \
  -alpn h2 -tls1_3 </dev/null 2>&1 | tr -d '\000' >"${RUNTIME}/tls13.log"
grep -q 'New, TLSv1.3' "${RUNTIME}/tls13.log"
grep -q 'ALPN protocol: h2' "${RUNTIME}/tls13.log"
if openssl s_client -connect 127.0.0.1:18443 -servername static.test \
  -tls1_2 </dev/null 2>&1 | grep -q 'New, TLSv1.2'; then
  echo 'TLS 1.2 was unexpectedly accepted' >&2
  exit 1
fi

proxy_response=$(curl --noproxy '*' -ksS --http2 \
  --resolve app.test:18443:127.0.0.1 \
  -H 'x-forwarded-for: 198.51.100.50, 10.0.0.2' \
  https://app.test:18443/hello)
jq -e '.headers["x-forwarded-for"] == "198.51.100.50"' \
  <<<"${proxy_response}" >/dev/null
jq -e '.headers["x-forwarded-proto"] == "https"' \
  <<<"${proxy_response}" >/dev/null

status=$(curl --noproxy '*' -ksS --http2 -o /dev/null -w '%{http_code}' \
  --resolve app.test:18443:127.0.0.1 \
  --data 'this-body-is-over-sixteen-bytes' https://app.test:18443/upload)
[[ "${status}" == "413" ]]

for _ in {1..4}; do
  status=$(curl --noproxy '*' -ksS --http2 -o /dev/null -w '%{http_code}' \
    --resolve vault.test:18443:127.0.0.1 \
    https://vault.test:18443/api/accounts/login)
  [[ "${status}" == "200" ]]
done
status=$(curl --noproxy '*' -ksS --http2 -o /dev/null -w '%{http_code}' \
  --resolve vault.test:18443:127.0.0.1 \
  https://vault.test:18443/api/accounts/login)
[[ "${status}" == "429" ]]

if curl --noproxy '*' -ksSI --http2 --resolve app.test:18443:127.0.0.1 \
  https://app.test:18443/hello | grep -qi '^server:'; then
  echo 'upstream Server header leaked' >&2
  exit 1
fi

status=$(curl --noproxy '*' -sS -o /dev/null -w '%{http_code}' \
  -H 'host: health.invalid' http://127.0.0.1:18080/nginx-health)
[[ "${status}" == "404" ]]

echo 'Pingora integration checks passed'
