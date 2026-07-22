#!/usr/bin/env bash
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
RUNTIME=/tmp/pingora-limit-isolation
BIN=${ROOT}/target/debug/pingora
GATEWAY_PID=
BACKEND_PID=
STREAM_PID=

cleanup() {
  for pid in "${STREAM_PID}" "${GATEWAY_PID}" "${BACKEND_PID}"; do
    if [[ -n "${pid}" ]]; then
      kill "${pid}" 2>/dev/null || true
      wait "${pid}" 2>/dev/null || true
    fi
  done
}
trap cleanup EXIT INT TERM

rm -rf "${RUNTIME}"
mkdir -p "${RUNTIME}"
cat >"${RUNTIME}/pingora.yaml" <<EOF
server:
  http_listen: ["127.0.0.1:80"]
  https_listen: []
  graceful_shutdown_timeout_seconds: 1
  health_socket: ${RUNTIME}/health.sock
trusted_proxies: ["127.0.0.0/8"]
upstreams:
  backend:
    address: "127.0.0.1:19997"
hosts:
  navidrome:
    domains: ["nav.test"]
    handler: navidrome-main
    upstream: backend
  vaultwarden:
    domains: ["vault.test"]
    handler: vaultwarden
    upstream: backend
route_limits:
  navidrome_stream:
    rate_per_second: 0
    active_requests: 1
  vaultwarden:
    active_requests: 1
EOF

LIMIT_MARKER="${RUNTIME}/stream-active" \
  python3 "${ROOT}/tests/limit_backend.py" >"${RUNTIME}/backend.stdout" \
  2>"${RUNTIME}/backend.stderr" &
BACKEND_PID=$!
RUST_LOG=warn "${BIN}" --config "${RUNTIME}/pingora.yaml" \
  >"${RUNTIME}/gateway.stdout" 2>"${RUNTIME}/gateway.stderr" &
GATEWAY_PID=$!
for _ in {1..100}; do
  if "${BIN}" --config "${RUNTIME}/pingora.yaml" --healthcheck >/dev/null 2>&1; then
    break
  fi
  sleep 0.1
done

curl --noproxy '*' -sS -o /dev/null -H 'host: nav.test' \
  http://127.0.0.1:80/stream &
STREAM_PID=$!
for _ in {1..100}; do
  [[ -f "${RUNTIME}/stream-active" ]] && break
  sleep 0.02
done
[[ -f "${RUNTIME}/stream-active" ]]

same_service=$(curl --noproxy '*' -sS -o /dev/null -w '%{http_code}' \
  -H 'host: nav.test' http://127.0.0.1:80/stream)
other_service=$(curl --noproxy '*' -sS -o /dev/null -w '%{http_code}' \
  -H 'host: vault.test' http://127.0.0.1:80/api)
[[ "${same_service}" == 429 ]]
[[ "${other_service}" == 200 ]]

wait "${STREAM_PID}"
STREAM_PID=
echo "service-specific active request limit isolation passed"
