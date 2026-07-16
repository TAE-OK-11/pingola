#!/usr/bin/env bash
set -euo pipefail

PINGORA_BIN=${1:?usage: pgo_train.sh PINGORA_BIN BACKEND_BIN CLIENT_BIN OUTPUT_DIR}
BACKEND_BIN=${2:?usage: pgo_train.sh PINGORA_BIN BACKEND_BIN CLIENT_BIN OUTPUT_DIR}
CLIENT_BIN=${3:?usage: pgo_train.sh PINGORA_BIN BACKEND_BIN CLIENT_BIN OUTPUT_DIR}
OUTPUT_DIR=${4:?usage: pgo_train.sh PINGORA_BIN BACKEND_BIN CLIENT_BIN OUTPUT_DIR}
REQUIRE_PROFILE=${PGO_REQUIRE_PROFILE:-true}
HTTP_PORT=${PGO_HTTP_PORT:-19080}
BACKEND_PORT=${PGO_BACKEND_PORT:-19000}
RUNTIME_DIR=${OUTPUT_DIR}/runtime
BACKEND_PID=
PINGORA_PID=

cleanup() {
  if [[ -n "${PINGORA_PID}" ]]; then
    kill -TERM "${PINGORA_PID}" >/dev/null 2>&1 || true
    wait "${PINGORA_PID}" >/dev/null 2>&1 || true
  fi
  if [[ -n "${BACKEND_PID}" ]]; then
    kill -TERM "${BACKEND_PID}" >/dev/null 2>&1 || true
    wait "${BACKEND_PID}" >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT INT TERM

install -d -m 0700 "${OUTPUT_DIR}" "${RUNTIME_DIR}"
cat >"${OUTPUT_DIR}/pingora.yaml" <<EOF
server:
  http_listen: ["127.0.0.1:${HTTP_PORT}"]
  https_listen: []
  health_socket: ${RUNTIME_DIR}/health.sock
  threads: 1
  upstream_keepalive_pool_size: 128
  downstream_keepalive_requests: 1000000
  max_retries: 0
  access_log: false
  http2_max_concurrent_streams: 128
  graceful_shutdown_timeout_seconds: 2
trusted_proxies: ["127.0.0.0/8"]
upstreams:
  backend:
    address: "127.0.0.1:${BACKEND_PORT}"
    connect_timeout_seconds: 2
    read_timeout_seconds: 60
    write_timeout_seconds: 60
    idle_timeout_seconds: 30
hosts:
  pgo:
    domains: ["pgo.test"]
    handler: vaultwarden
    upstream: backend
route_limits:
  vaultwarden:
    rate_per_second: 0
    active_requests: 0
EOF

"${BACKEND_BIN}" --port "${BACKEND_PORT}" >"${OUTPUT_DIR}/backend.log" 2>&1 &
BACKEND_PID=$!
for _ in {1..100}; do
  if (exec 3<>"/dev/tcp/127.0.0.1/${BACKEND_PORT}") 2>/dev/null; then
    exec 3>&-
    break
  fi
  sleep 0.05
done
kill -0 "${BACKEND_PID}"

LLVM_PROFILE_FILE="${OUTPUT_DIR}/pingora-%p-%m.profraw" \
  "${PINGORA_BIN}" --config "${OUTPUT_DIR}/pingora.yaml" \
  >"${OUTPUT_DIR}/pingora.log" 2>&1 &
PINGORA_PID=$!
for _ in {1..100}; do
  if (exec 3<>"/dev/tcp/127.0.0.1/${HTTP_PORT}") 2>/dev/null; then
    exec 3>&-
    break
  fi
  sleep 0.05
done
kill -0 "${PINGORA_PID}"

"${CLIENT_BIN}" --port "${HTTP_PORT}" --threads 1 --requests-per-thread 5000
"${CLIENT_BIN}" --port "${HTTP_PORT}" --threads 8 --requests-per-thread 10000

kill -TERM "${PINGORA_PID}"
wait "${PINGORA_PID}"
PINGORA_PID=
if [[ "${REQUIRE_PROFILE}" == true ]]; then
  compgen -G "${OUTPUT_DIR}/*.profraw" >/dev/null
elif [[ "${REQUIRE_PROFILE}" != false ]]; then
  echo "PGO_REQUIRE_PROFILE must be true or false" >&2
  exit 2
fi
