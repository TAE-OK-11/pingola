#!/usr/bin/env bash
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
RUNTIME=/tmp/pingora-listeners
BIN=${ROOT}/target/debug/pingora
PID=

cleanup() {
  if [[ -n "${PID}" ]]; then
    kill "${PID}" 2>/dev/null || true
    wait "${PID}" 2>/dev/null || true
  fi
}
trap cleanup EXIT INT TERM

write_http_config() {
  local name=$1
  local listeners=$2
  cat >"${RUNTIME}/${name}.yaml" <<EOF
server:
  http_listen: ${listeners}
  https_listen: []
  health_socket: ${RUNTIME}/${name}.sock
  health_details: true
  graceful_shutdown_timeout_seconds: 1
trusted_proxies: ["127.0.0.0/8", "::1/128"]
upstreams:
  unavailable:
    address: "127.0.0.1:9"
hosts:
  app:
    domains: ["app.test"]
    handler: vaultwarden
    upstream: unavailable
EOF
}

start() {
  local name=$1
  RUST_LOG=info "${BIN}" --config "${RUNTIME}/${name}.yaml" \
    >"${RUNTIME}/${name}.stdout" 2>"${RUNTIME}/${name}.stderr" &
  PID=$!
  for _ in {1..100}; do
    if "${BIN}" --config "${RUNTIME}/${name}.yaml" --healthcheck \
      >/dev/null 2>"${RUNTIME}/${name}.health-error"; then
      return
    fi
    kill -0 "${PID}" 2>/dev/null || break
    sleep 0.1
  done
  cat "${RUNTIME}/${name}.stderr" >&2
  cat "${RUNTIME}/${name}.health-error" >&2
  exit 1
}

stop() {
  cleanup
  PID=
}

rm -rf "${RUNTIME}"
mkdir -p "${RUNTIME}"

write_http_config ipv4 '["127.0.0.1:80"]'
"${BIN}" --config "${RUNTIME}/ipv4.yaml" --check-bind >/dev/null
start ipv4
curl --noproxy '*' -fsS -H 'host: health.invalid' \
  http://127.0.0.1:80/pingora-health -D "${RUNTIME}/ipv4.headers" -o /dev/null
grep -qi '^x-proxy-product: Pingora' "${RUNTIME}/ipv4.headers"
stop

write_http_config ipv6 '["[::1]:80"]'
"${BIN}" --config "${RUNTIME}/ipv6.yaml" --check-bind >/dev/null
start ipv6
curl --noproxy '*' -gfsS -H 'host: health.invalid' \
  'http://[::1]:80/pingora-health' -o /dev/null
stop

write_http_config dual '["0.0.0.0:80", "[::]:80"]'
"${BIN}" --config "${RUNTIME}/dual.yaml" --check-bind \
  >"${RUNTIME}/dual-check.log" 2>&1
grep -q 'IPV6_V6ONLY=true' "${RUNTIME}/dual-check.log"
start dual
curl --noproxy '*' -fsS -H 'host: health.invalid' \
  http://127.0.0.1:80/pingora-ready -o /dev/null
curl --noproxy '*' -gfsS -H 'host: health.invalid' \
  'http://[::1]:80/pingora-live' -o /dev/null
for _ in 1 2; do
  curl --noproxy '*' -fsS -H 'host: health.invalid' \
    http://127.0.0.1:80/pingola-health -o /dev/null
done
status=$(curl --noproxy '*' -sS -o /dev/null -w '%{http_code}' \
  -H 'host: health.invalid' http://127.0.0.1:80/nginx-health)
[[ "${status}" == 404 ]]
details_status=$(curl --noproxy '*' -sS -o "${RUNTIME}/details.json" -w '%{http_code}' \
  -H 'host: health.invalid' 'http://127.0.0.1:80/pingora-health/details?upstreams=1')
[[ "${details_status}" == 503 ]]
jq -e '.product == "Pingora" and .readiness == false and .upstreams.unavailable == false' \
  "${RUNTIME}/details.json" >/dev/null
stop
[[ $(grep -c '/pingola-health is deprecated' "${RUNTIME}/dual.stderr") -eq 1 ]]

openssl req -x509 -newkey rsa:2048 -nodes -days 1 \
  -subj '/CN=health.test' -addext 'subjectAltName=DNS:health.test' \
  -keyout "${RUNTIME}/key.pem" -out "${RUNTIME}/cert.pem" >/dev/null 2>&1
cat >"${RUNTIME}/https-only.yaml" <<EOF
server:
  http_listen: []
  https_listen: ["[::1]:443"]
  certificate: ${RUNTIME}/cert.pem
  private_key: ${RUNTIME}/key.pem
  health_socket: ${RUNTIME}/https-only.sock
  graceful_shutdown_timeout_seconds: 1
trusted_proxies: ["::1/128"]
upstreams:
  unavailable:
    address: "127.0.0.1:9"
hosts:
  app:
    domains: ["app.test"]
    handler: vaultwarden
    upstream: unavailable
EOF
"${BIN}" --config "${RUNTIME}/https-only.yaml" --check-bind >/dev/null
start https-only
"${BIN}" --config "${RUNTIME}/https-only.yaml" --healthcheck
curl --noproxy '*' -gkfsS --http2 --resolve health.test:443:[::1] \
  https://health.test:443/pingora-health -o /dev/null
stop

echo "IPv4, IPv6, dual-stack, HTTP-only, and HTTPS-only health tests passed"
