#!/usr/bin/env bash
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
RUNTIME=/tmp/pingora-retries
BIN=${ROOT}/target/debug/pingora
PID=
BACKEND_PID=

cleanup() {
  if [[ -n "${PID}" ]]; then
    kill "${PID}" 2>/dev/null || true
    wait "${PID}" 2>/dev/null || true
  fi
  if [[ -n "${BACKEND_PID}" ]]; then
    kill "${BACKEND_PID}" 2>/dev/null || true
    wait "${BACKEND_PID}" 2>/dev/null || true
  fi
}
trap cleanup EXIT INT TERM

write_config() {
  local name=$1
  local retries=$2
  local listen=$3
  local upstream=$4
  cat >"${RUNTIME}/${name}.yaml" <<EOF
server:
  http_listen: ["127.0.0.1:${listen}"]
  https_listen: []
  max_retries: ${retries}
  graceful_shutdown_timeout_seconds: 1
  health_socket: ${RUNTIME}/${name}.sock
trusted_proxies: ["127.0.0.0/8"]
upstreams:
  retry:
    address: "${upstream}"
hosts:
  retry:
    domains: ["retry.test"]
    handler: vaultwarden
    upstream: retry
EOF
}

start() {
  local name=$1
  RUST_LOG=warn "${BIN}" --config "${RUNTIME}/${name}.yaml" \
    >"${RUNTIME}/${name}.stdout" 2>"${RUNTIME}/${name}.stderr" &
  PID=$!
  for _ in {1..100}; do
    if "${BIN}" --config "${RUNTIME}/${name}.yaml" --healthcheck >/dev/null 2>&1; then
      return
    fi
    kill -0 "${PID}" 2>/dev/null || break
    sleep 0.1
  done
  cat "${RUNTIME}/${name}.stderr" >&2
  exit 1
}

stop() {
  kill "${PID}" 2>/dev/null || true
  wait "${PID}" 2>/dev/null || true
  PID=
}

request() {
  local method=$1
  local port=$2
  shift 2
  local status
  status=$(curl --noproxy '*' -sS -o /dev/null -w '%{http_code}' \
    -X "${method}" -H 'host: retry.test' "$@" "http://127.0.0.1:${port}/retry")
  [[ "${status}" == 502 ]]
}

rm -rf "${RUNTIME}"
mkdir -p "${RUNTIME}"

for retries in 0 1 2; do
  port=80
  name=max-${retries}
  write_config "${name}" "${retries}" "${port}" "127.0.0.1:19999"
  start "${name}"
  request GET "${port}"
  stop
  attempts=$(grep -c 'upstream connect failure.*method=GET' "${RUNTIME}/${name}.stderr")
  [[ "${attempts}" -eq $((retries + 1)) ]]
  grep -q "attempt=$((retries + 1)) configured_retries=${retries} retry=false method=GET" \
    "${RUNTIME}/${name}.stderr"
done

# Non-idempotent and body-bearing requests are never replayed.
write_config unsafe-body 2 80 "127.0.0.1:19999"
start unsafe-body
request POST 80 --data-binary 'streaming-body-must-not-repeat'
request PUT 80 --data-binary 'streaming-body-must-not-repeat'
stop
[[ $(grep -c 'upstream connect failure.*method=POST' "${RUNTIME}/unsafe-body.stderr") -eq 1 ]]
[[ $(grep -c 'upstream connect failure.*method=PUT' "${RUNTIME}/unsafe-body.stderr") -eq 1 ]]
! grep -Eq 'retry=true method=(POST|PUT)' "${RUNTIME}/unsafe-body.stderr"

# HTTP 503 is an upstream response, not a connection failure, and is returned
# exactly once without replaying the request.
: >"${RUNTIME}/backend-count.log"
RETRY_COUNT_FILE="${RUNTIME}/backend-count.log" \
  python3 "${ROOT}/tests/retry_backend.py" >"${RUNTIME}/backend.stdout" \
  2>"${RUNTIME}/backend.stderr" &
BACKEND_PID=$!
for _ in {1..50}; do
  if curl --noproxy '*' -sS http://127.0.0.1:19998/ready -o /dev/null 2>/dev/null; then
    break
  fi
  sleep 0.1
done
: >"${RUNTIME}/backend-count.log"
write_config status-503 2 80 "127.0.0.1:19998"
start status-503
status=$(curl --noproxy '*' -sS -o /dev/null -w '%{http_code}' \
  -H 'host: retry.test' http://127.0.0.1:80/retry)
[[ "${status}" == 503 ]]
stop
[[ $(wc -l <"${RUNTIME}/backend-count.log") -eq 1 ]]

echo "max_retries 0/1/2 and unsafe-body retry tests passed"
