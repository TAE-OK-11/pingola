#!/usr/bin/env bash
set -euo pipefail

# shellcheck source=bench/pgo_train_scale.sh
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/pgo_train_scale.sh"

PINGORA_BIN=${1:?usage: pgo_train_upstream_h2.sh PINGORA_BIN ORIGIN_BIN BACKEND_BIN OUTPUT_DIR}
ORIGIN_BIN=${2:?usage: pgo_train_upstream_h2.sh PINGORA_BIN ORIGIN_BIN BACKEND_BIN OUTPUT_DIR}
BACKEND_BIN=${3:?usage: pgo_train_upstream_h2.sh PINGORA_BIN ORIGIN_BIN BACKEND_BIN OUTPUT_DIR}
OUTPUT_DIR=${4:?usage: pgo_train_upstream_h2.sh PINGORA_BIN ORIGIN_BIN BACKEND_BIN OUTPUT_DIR}

ECDSA_CURVE=${PGO_ECDSA_CURVE:-prime256v1}
REQUIRE_PROFILE=${PGO_REQUIRE_PROFILE:-true}
ROUND=${PGO_TRAIN_ROUND:-1}
TARGET_HTTPS_PORT=${PGO_UPSTREAM_H2_TARGET_HTTPS_PORT:-19447}
ORIGIN_HTTPS_PORT=${PGO_UPSTREAM_H2_ORIGIN_HTTPS_PORT:-19448}
BACKEND_PORT=${PGO_UPSTREAM_H2_BACKEND_PORT:-19002}
RUNTIME_DIR=${OUTPUT_DIR}/runtime
BACKEND_PID=
ORIGIN_PID=
PINGORA_PID=

case "${ECDSA_CURVE}" in
  prime256v1|secp384r1) ;;
  *) echo "unsupported ECDSA curve: ${ECDSA_CURVE}" >&2; exit 2 ;;
esac

cleanup() {
  for pid in "${PINGORA_PID}" "${ORIGIN_PID}" "${BACKEND_PID}"; do
    if [[ -n "${pid}" ]]; then
      kill -TERM "${pid}" >/dev/null 2>&1 || true
      wait "${pid}" >/dev/null 2>&1 || true
    fi
  done
}
trap cleanup EXIT INT TERM

wait_tcp() {
  local port=$1 pid=$2 name=$3
  for _ in {1..200}; do
    if (exec 3<>"/dev/tcp/127.0.0.1/${port}") 2>/dev/null; then exec 3>&-; return 0; fi
    kill -0 "${pid}" 2>/dev/null || { echo "${name} exited before readiness" >&2; return 1; }
    sleep 0.05
  done
  echo "${name} did not become ready on port ${port}" >&2
  return 1
}

show_failure_logs() {
  local file
  for file in target.log origin.log backend.log; do
    if [[ -s "${OUTPUT_DIR}/${file}" ]]; then
      echo "=== ${file} (tail) ===" >&2
      tail -n 160 "${OUTPUT_DIR}/${file}" >&2 || true
    fi
  done
}

run_h2load() {
  local name=$1; shift
  h2load "$@" >"${OUTPUT_DIR}/${name}.log" 2>&1
  grep -Eq '0 failed, 0 errored' "${OUTPUT_DIR}/${name}.log" || {
    echo "upstream H2 workload failed: ${name}" >&2
    sed -n '1,180p' "${OUTPUT_DIR}/${name}.log" >&2
    show_failure_logs
    exit 1
  }
}

rm -rf "${RUNTIME_DIR}"
install -d -m 0700 "${OUTPUT_DIR}" "${RUNTIME_DIR}"
openssl genpkey -algorithm EC -pkeyopt "ec_paramgen_curve:${ECDSA_CURVE}" \
  -out "${RUNTIME_DIR}/key.pem" >"${OUTPUT_DIR}/openssl-key.log" 2>&1
openssl req -new -x509 -sha256 -days 1 -key "${RUNTIME_DIR}/key.pem" \
  -subj '/CN=pgo.test' \
  -addext 'subjectAltName=DNS:pgo.test,DNS:origin.test,DNS:music.test' \
  -out "${RUNTIME_DIR}/cert.pem" >"${OUTPUT_DIR}/openssl-cert.log" 2>&1
chmod 0600 "${RUNTIME_DIR}/key.pem" "${RUNTIME_DIR}/cert.pem"

cat >"${OUTPUT_DIR}/origin.yaml" <<EOF_ORIGIN
server:
  http_listen: []
  https_listen: ["127.0.0.1:${ORIGIN_HTTPS_PORT}"]
  certificate: ${RUNTIME_DIR}/cert.pem
  private_key: ${RUNTIME_DIR}/key.pem
  health_socket: ${RUNTIME_DIR}/origin-health.sock
  threads: 2
  access_log: false
  downstream_keepalive_requests: 1000000
  http2_max_concurrent_streams: 128
  graceful_shutdown_timeout_seconds: 2
trusted_proxies: ["127.0.0.0/8"]
upstreams:
  backend:
    address: "127.0.0.1:${BACKEND_PORT}"
    protocol: http1
    idle_timeout_seconds: 30
hosts:
  profile: { domains: ["origin.test", "pgo.test"], handler: vaultwarden, upstream: backend }
  stream: { domains: ["music.test"], handler: navidrome-main, upstream: backend }
route_limits:
  vaultwarden: { rate_per_second: 0, active_requests: 0 }
  navidrome_stream: { rate_per_second: 0, active_requests: 0 }
EOF_ORIGIN

cat >"${OUTPUT_DIR}/target.yaml" <<EOF_TARGET
server:
  http_listen: []
  https_listen: ["127.0.0.1:${TARGET_HTTPS_PORT}"]
  certificate: ${RUNTIME_DIR}/cert.pem
  private_key: ${RUNTIME_DIR}/key.pem
  health_socket: ${RUNTIME_DIR}/target-health.sock
  threads: 2
  access_log: false
  upstream_keepalive_pool_size: 128
  downstream_keepalive_requests: 1000000
  http2_max_concurrent_streams: 128
  graceful_shutdown_timeout_seconds: 2
trusted_proxies: ["127.0.0.0/8"]
upstreams:
  origin_h2:
    address: "127.0.0.1:${ORIGIN_HTTPS_PORT}"
    tls: true
    sni: origin.test
    verify_certificate: false
    http2_max_concurrent_streams: 128
    connect_timeout_seconds: 3
    read_timeout_seconds: 60
    write_timeout_seconds: 60
    idle_timeout_seconds: 30
hosts:
  profile: { domains: ["pgo.test"], handler: vaultwarden, upstream: origin_h2 }
  stream: { domains: ["music.test"], handler: navidrome-main, upstream: origin_h2 }
route_limits:
  vaultwarden: { rate_per_second: 0, active_requests: 0 }
  navidrome_stream: { rate_per_second: 0, active_requests: 0 }
EOF_TARGET

"${BACKEND_BIN}" --port "${BACKEND_PORT}" >"${OUTPUT_DIR}/backend.log" 2>&1 &
BACKEND_PID=$!
wait_tcp "${BACKEND_PORT}" "${BACKEND_PID}" backend

"${ORIGIN_BIN}" --config "${OUTPUT_DIR}/origin.yaml" >"${OUTPUT_DIR}/origin.log" 2>&1 &
ORIGIN_PID=$!
wait_tcp "${ORIGIN_HTTPS_PORT}" "${ORIGIN_PID}" origin

CHECK_PATTERN="${RUNTIME_DIR}/check-%p-%m.profraw"
LLVM_PROFILE_FILE="${CHECK_PATTERN}" "${PINGORA_BIN}" --config "${OUTPUT_DIR}/target.yaml" --check \
  >"${OUTPUT_DIR}/target-check.log" 2>&1
rm -f "${RUNTIME_DIR}"/check-*.profraw

LLVM_PROFILE_FILE="${OUTPUT_DIR}/pingora-upstream-h2-r${ROUND}-%p-%m.profraw" \
  "${PINGORA_BIN}" --config "${OUTPUT_DIR}/target.yaml" >"${OUTPUT_DIR}/target.log" 2>&1 &
PINGORA_PID=$!
wait_tcp "${TARGET_HTTPS_PORT}" "${PINGORA_PID}" target

run_h2load small -n "$(pgo_train_scale 6000)" -c 4 -m 16 -w 16 -W 20 --sni pgo.test \
  -H 'host: pgo.test' -H 'accept-encoding: identity' \
  "https://127.0.0.1:${TARGET_HTTPS_PORT}/json/512"
run_h2load medium -n "$(pgo_train_scale 3000)" -c 4 -m 16 -w 16 -W 20 --sni pgo.test \
  -H 'host: pgo.test' -H 'accept-encoding: identity' \
  "https://127.0.0.1:${TARGET_HTTPS_PORT}/bytes/4096"
run_h2load bulk-5m -n "$(pgo_train_scale 64)" -c 4 -m 8 -w 16 -W 20 --sni pgo.test \
  -H 'host: pgo.test' -H 'accept-encoding: identity' \
  "https://127.0.0.1:${TARGET_HTTPS_PORT}/bytes/5242880"
run_h2load bulk-30m -n "$(pgo_train_scale 24)" -c 1 -m 1 -w 16 -W 20 --sni pgo.test \
  -H 'host: pgo.test' -H 'accept-encoding: identity' \
  "https://127.0.0.1:${TARGET_HTTPS_PORT}/bytes/31457280"

UPSTREAM_BULK_N="$(pgo_train_scale 160)"
UPSTREAM_CHUNKED_N="$(pgo_train_scale 256)"
if [[ "${PGO_TRAIN_FAST:-off}" == on ]]; then
  (( UPSTREAM_BULK_N > 24 )) && UPSTREAM_BULK_N=24
  (( UPSTREAM_CHUNKED_N > 32 )) && UPSTREAM_CHUNKED_N=32
fi

run_h2load large-json -n "$(pgo_train_scale 1000)" -c 4 -m 8 -w 16 -W 20 --sni pgo.test \
  -H 'host: pgo.test' -H 'accept-encoding: identity' \
  "https://127.0.0.1:${TARGET_HTTPS_PORT}/json/65536"
run_h2load bulk-512k -n "${UPSTREAM_BULK_N}" -c 1 -m 1 -w 16 -W 20 --sni pgo.test \
  -H 'host: pgo.test' -H 'accept-encoding: identity' \
  "https://127.0.0.1:${TARGET_HTTPS_PORT}/bytes/524288"
run_h2load chunked-128k -n "${UPSTREAM_CHUNKED_N}" -c 1 -m 1 -w 16 -W 20 --sni pgo.test \
  -H 'host: pgo.test' -H 'accept-encoding: identity' \
  "https://127.0.0.1:${TARGET_HTTPS_PORT}/stream/131072"

for index in $(seq 1 "$(pgo_train_scale 8)"); do
  sleep 1.5
  run_h2load "resume-${index}" -n 32 -c 1 -m 1 --sni pgo.test \
    -H 'host: pgo.test' -H 'accept-encoding: identity' \
    "https://127.0.0.1:${TARGET_HTTPS_PORT}/json/512"
done

cat >"${OUTPUT_DIR}/workload.txt" <<EOF_WORKLOAD
scenario=upstream-h2
round=${ROUND}
small_requests=6000
medium_requests=3000
large_json_requests=1000
bulk_512k_requests=160
chunked_128k_requests=256
bulk_5m_requests=64
bulk_30m_requests=24
resumption_cycles=8
resumption_requests=256
EOF_WORKLOAD

kill -TERM "${PINGORA_PID}"
wait "${PINGORA_PID}"
PINGORA_PID=

if [[ "${REQUIRE_PROFILE}" == true ]]; then
  compgen -G "${OUTPUT_DIR}/*.profraw" >/dev/null
elif [[ "${REQUIRE_PROFILE}" != false ]]; then
  echo 'PGO_REQUIRE_PROFILE must be true or false' >&2
  exit 2
fi
