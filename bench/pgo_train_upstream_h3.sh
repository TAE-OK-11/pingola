#!/usr/bin/env bash
set -euo pipefail

# shellcheck source=bench/pgo_train_scale.sh
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/pgo_train_scale.sh"

PINGORA_BIN=${1:?usage: pgo_train_upstream_h3.sh PINGORA_BIN ORIGIN_BIN BACKEND_BIN HTTP3_PROBE_BIN OUTPUT_DIR CC}
ORIGIN_BIN=${2:?usage: pgo_train_upstream_h3.sh PINGORA_BIN ORIGIN_BIN BACKEND_BIN HTTP3_PROBE_BIN OUTPUT_DIR CC}
BACKEND_BIN=${3:?usage: pgo_train_upstream_h3.sh PINGORA_BIN ORIGIN_BIN BACKEND_BIN HTTP3_PROBE_BIN OUTPUT_DIR CC}
HTTP3_PROBE_BIN=${4:?usage: pgo_train_upstream_h3.sh PINGORA_BIN ORIGIN_BIN BACKEND_BIN HTTP3_PROBE_BIN OUTPUT_DIR CC}
OUTPUT_DIR=${5:?usage: pgo_train_upstream_h3.sh PINGORA_BIN ORIGIN_BIN BACKEND_BIN HTTP3_PROBE_BIN OUTPUT_DIR CC}
CC=${6:?usage: pgo_train_upstream_h3.sh PINGORA_BIN ORIGIN_BIN BACKEND_BIN HTTP3_PROBE_BIN OUTPUT_DIR CC}

ECDSA_CURVE=${PGO_ECDSA_CURVE:-prime256v1}
REQUIRE_PROFILE=${PGO_REQUIRE_PROFILE:-true}
ROUND=${PGO_TRAIN_ROUND:-1}
TARGET_HTTPS_PORT=${PGO_UPSTREAM_TARGET_HTTPS_PORT:-19445}
TARGET_H3_PORT=${PGO_UPSTREAM_TARGET_H3_PORT:-19446}
ORIGIN_H3_PORT=${PGO_UPSTREAM_ORIGIN_H3_PORT:-19444}
ORIGIN_INTERNAL_PORT=${PGO_UPSTREAM_ORIGIN_INTERNAL_PORT:-18081}
BACKEND_PORT=${PGO_UPSTREAM_BACKEND_PORT:-19001}
RUNTIME_DIR=${OUTPUT_DIR}/runtime
BACKEND_PID=
ORIGIN_PID=
PINGORA_PID=

case "${CC}" in
  bbr2) BBR2=true ;;
  cubic) BBR2=false ;;
  *) echo "unsupported upstream H3 congestion control: ${CC}" >&2; exit 2 ;;
esac
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
    echo "upstream H3 workload failed: ${name}" >&2
    sed -n '1,180p' "${OUTPUT_DIR}/${name}.log" >&2
    show_failure_logs
    exit 1
  }
}

ensure_h3_ready() {
  for _ in {1..120}; do
    if "${HTTP3_PROBE_BIN}" "127.0.0.1:${TARGET_H3_PORT}" pgo.test /json/512 1 \
      >"${OUTPUT_DIR}/h3-ready.out" 2>"${OUTPUT_DIR}/h3-ready.log"; then
      return 0
    fi
    sleep 0.1
  done
  echo "target HTTP/3 listener not ready before probe workload" >&2
  show_failure_logs
  return 1
}

wait_target_h3_ready() {
  for _ in {1..200}; do
    if "${HTTP3_PROBE_BIN}" "127.0.0.1:${TARGET_H3_PORT}" pgo.test /json/512 1 \
      >"${OUTPUT_DIR}/target-h3-ready.out" 2>"${OUTPUT_DIR}/target-h3-ready.log"; then
      return 0
    fi
    kill -0 "${PINGORA_PID}" 2>/dev/null || return 1
    sleep 0.05
  done
  return 1
}

restart_target_h3_listener() {
  echo "restarting target Pingora for a fresh downstream HTTP/3 listener" >&2
  if [[ -n "${PINGORA_PID}" ]]; then
    kill -TERM "${PINGORA_PID}" >/dev/null 2>&1 || true
    wait "${PINGORA_PID}" >/dev/null 2>&1 || true
    PINGORA_PID=
  fi
  LLVM_PROFILE_FILE="${OUTPUT_DIR}/pingora-upstream-${CC}-r${ROUND}-%p-%m.profraw" \
    "${PINGORA_BIN}" --config "${OUTPUT_DIR}/target.yaml" >>"${OUTPUT_DIR}/target.log" 2>&1 &
  PINGORA_PID=$!
  wait_tcp "${TARGET_HTTPS_PORT}" "${PINGORA_PID}" target
  wait_target_h3_ready
}

run_h3_probe() {
  local name=$1
  local authority=$2
  local path=$3
  local requests=$4
  local optional1=${5:-}
  local optional2=${6:-}
  local attempt
  local concurrency=1
  local accept_encoding=
  local -a probe_args=("127.0.0.1:${TARGET_H3_PORT}" "${authority}" "${path}" "${requests}")

  if [[ -n "${optional2}" ]]; then
    concurrency="${optional1}"
    accept_encoding="${optional2}"
  elif [[ -n "${optional1}" ]]; then
    if [[ "${optional1}" =~ ^[0-9]+$ ]]; then
      concurrency="${optional1}"
    else
      accept_encoding="${optional1}"
    fi
  fi

  probe_args+=("${concurrency}")
  if [[ -n "${accept_encoding}" ]]; then
    probe_args+=("${accept_encoding}")
  fi

  for attempt in 1 2 3 4 5; do
    if ! ensure_h3_ready; then
      restart_target_h3_listener || exit 1
    fi
    if "${HTTP3_PROBE_BIN}" "${probe_args[@]}" \
      >"${OUTPUT_DIR}/${name}.out" 2>"${OUTPUT_DIR}/${name}.log"; then
      return 0
    fi
    if grep -q 'controller is closed' "${OUTPUT_DIR}/${name}.log" 2>/dev/null; then
      restart_target_h3_listener || exit 1
      sleep 1
    else
      sleep 0.5
    fi
  done

  echo "upstream H3 direct-gateway workload failed: ${name}" >&2
  sed -n '1,160p' "${OUTPUT_DIR}/${name}.log" >&2
  show_failure_logs
  exit 1
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
  https_listen: []
  http3_listen: ["127.0.0.1:${ORIGIN_H3_PORT}"]
  http3_max_idle_timeout_seconds: 30
  http3_max_concurrent_streams: 128
  http3_enable_early_data: true
  http3_stateless_retry: false
  certificate: ${RUNTIME_DIR}/cert.pem
  private_key: ${RUNTIME_DIR}/key.pem
  health_socket: ${RUNTIME_DIR}/origin-health.sock
  threads: 2
  access_log: false
  downstream_keepalive_requests: 1000000
  http3_max_requests_per_connection: 1000000
  downstream_max_connections: 4096
  static_active_requests_per_client: 1000000
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
  http3_listen: ["127.0.0.1:${TARGET_H3_PORT}"]
  http3_max_idle_timeout_seconds: 60
  http3_max_concurrent_streams: 128
  certificate: ${RUNTIME_DIR}/cert.pem
  private_key: ${RUNTIME_DIR}/key.pem
  health_socket: ${RUNTIME_DIR}/target-health.sock
  threads: 2
  access_log: false
  upstream_keepalive_pool_size: 128
  downstream_keepalive_requests: 1000000
  http2_max_concurrent_streams: 128
  http3_max_requests_per_connection: 1000000
  graceful_shutdown_timeout_seconds: 2
trusted_proxies: ["127.0.0.0/8"]
upstreams:
  origin_h3:
    address: "127.0.0.1:${ORIGIN_H3_PORT}"
    tls: true
    protocol: http3
    sni: origin.test
    verify_certificate: false
    http3_bbr2: ${BBR2}
    http3_early_data: true
    http3_max_concurrent_streams: 128
    connect_timeout_seconds: 3
    read_timeout_seconds: 60
    write_timeout_seconds: 60
    idle_timeout_seconds: 3
hosts:
  profile: { domains: ["pgo.test"], handler: vaultwarden, upstream: origin_h3 }
  stream: { domains: ["music.test"], handler: navidrome-main, upstream: origin_h3 }
route_limits:
  vaultwarden: { rate_per_second: 0, active_requests: 0 }
  navidrome_stream: { rate_per_second: 0, active_requests: 0 }
EOF_TARGET

"${BACKEND_BIN}" --port "${BACKEND_PORT}" >"${OUTPUT_DIR}/backend.log" 2>&1 &
BACKEND_PID=$!
wait_tcp "${BACKEND_PORT}" "${BACKEND_PID}" backend

"${ORIGIN_BIN}" --config "${OUTPUT_DIR}/origin.yaml" >"${OUTPUT_DIR}/origin.log" 2>&1 &
ORIGIN_PID=$!
ready=false
for _ in {1..200}; do
  if "${HTTP3_PROBE_BIN}" "127.0.0.1:${ORIGIN_H3_PORT}" origin.test /json/512 \
      >"${OUTPUT_DIR}/origin-readiness.out" 2>"${OUTPUT_DIR}/origin-readiness.log"; then
    ready=true; break
  fi
  kill -0 "${ORIGIN_PID}" 2>/dev/null || { sed -n '1,220p' "${OUTPUT_DIR}/origin.log" >&2; exit 1; }
  sleep 0.05
done
[[ "${ready}" == true ]] || { echo "H3 origin readiness failed" >&2; exit 1; }

CHECK_PATTERN="${RUNTIME_DIR}/check-%p-%m.profraw"
LLVM_PROFILE_FILE="${CHECK_PATTERN}" "${PINGORA_BIN}" --config "${OUTPUT_DIR}/target.yaml" --check \
  >"${OUTPUT_DIR}/target-check.log" 2>&1
rm -f "${RUNTIME_DIR}"/check-*.profraw

LLVM_PROFILE_FILE="${OUTPUT_DIR}/pingora-upstream-${CC}-r${ROUND}-%p-%m.profraw" \
  "${PINGORA_BIN}" --config "${OUTPUT_DIR}/target.yaml" >"${OUTPUT_DIR}/target.log" 2>&1 &
PINGORA_PID=$!
wait_tcp "${TARGET_HTTPS_PORT}" "${PINGORA_PID}" target

h3_ready=false
for _ in {1..200}; do
  if wait_target_h3_ready; then
    h3_ready=true
    break
  fi
  kill -0 "${PINGORA_PID}" 2>/dev/null || { show_failure_logs; exit 1; }
  sleep 0.05
done
[[ "${h3_ready}" == true ]] || {
  echo "target HTTP/3 listener did not become ready on 127.0.0.1:${TARGET_H3_PORT}" >&2
  exit 1
}

# Run direct-gateway H3 probes while the downstream QUIC controller is fresh.
# Heavy zstd/json probes can leave the listener up but reject new QUIC sessions.
H3_JSON_PROBES=2
H3_JSON_REQUESTS=128
if [[ "${PGO_TRAIN_FAST:-off}" == on ]]; then
  H3_JSON_REQUESTS=64
fi
for index in $(seq 1 "${H3_JSON_PROBES}"); do
  run_h3_probe "h3-downstream-json-${index}" pgo.test /json/512 "${H3_JSON_REQUESTS}"
done
restart_target_h3_listener
run_h3_probe h3-downstream-stream music.test /stream/524288 "$(pgo_train_scale 8)"
run_h3_probe h3-downstream-bulk pgo.test /bytes/262144 "$(pgo_train_scale 32)"
restart_target_h3_listener
run_h3_probe h3-downstream-compress pgo.test /json/65536 "$(pgo_train_scale 32)" "zstd"

run_h2load small -n "$(pgo_train_scale 6000)" -c 4 -m 16 -w 16 -W 20 --sni pgo.test \
  -H 'host: pgo.test' -H 'accept-encoding: identity' \
  "https://127.0.0.1:${TARGET_HTTPS_PORT}/json/512"
run_h2load medium -n "$(pgo_train_scale 3000)" -c 4 -m 16 -w 16 -W 20 --sni pgo.test \
  -H 'host: pgo.test' -H 'accept-encoding: identity' \
  "https://127.0.0.1:${TARGET_HTTPS_PORT}/bytes/4096"

run_h2load large-json -n "$(pgo_train_scale 1000)" -c 4 -m 8 -w 16 -W 20 --sni pgo.test \
  -H 'host: pgo.test' -H 'accept-encoding: identity' \
  "https://127.0.0.1:${TARGET_HTTPS_PORT}/json/65536"

# Prefer a Content-Length bulk path for the majority of transfer training: it
# matches large object/audio/range-style responses more closely than the
# synthetic chunked fixture. Keep a separate chunked workload so incremental
# response forwarding and bodyless fast paths remain represented in PGO.
# Both workloads are serialized to avoid teaching the profile a single-thread
# synthetic queue-overflow artifact rather than steady production forwarding.
# Serial bulk transfers dominate publish wall time; cap them harder in fast mode.
UPSTREAM_BULK_N="$(pgo_train_scale 160)"
UPSTREAM_CHUNKED_N="$(pgo_train_scale 256)"
if [[ "${PGO_TRAIN_FAST:-off}" == on ]]; then
  (( UPSTREAM_BULK_N > 24 )) && UPSTREAM_BULK_N=24
  (( UPSTREAM_CHUNKED_N > 32 )) && UPSTREAM_CHUNKED_N=32
fi

run_h2load bulk-512k -n "${UPSTREAM_BULK_N}" -c 1 -m 1 -w 16 -W 20 --sni pgo.test \
  -H 'host: pgo.test' -H 'accept-encoding: identity' \
  "https://127.0.0.1:${TARGET_HTTPS_PORT}/bytes/524288"
run_h2load chunked-128k -n "${UPSTREAM_CHUNKED_N}" -c 1 -m 1 -w 16 -W 20 --sni pgo.test \
  -H 'host: pgo.test' -H 'accept-encoding: identity' \
  "https://127.0.0.1:${TARGET_HTTPS_PORT}/stream/131072"

# The target uses a deliberately short three-second upstream QUIC idle timeout
# only for this training fixture. Waiting past it repeatedly exercises reconnect,
# ticket caching/resumption, and replay-safe early-data paths without risking
# spurious idle expiry during the transfer workload above.
for index in $(seq 1 "$(pgo_train_scale 8)"); do
  sleep 3.3
  run_h2load "resume-${index}" -n 32 -c 1 -m 1 --sni pgo.test \
    -H 'host: pgo.test' -H 'accept-encoding: identity' \
    "https://127.0.0.1:${TARGET_HTTPS_PORT}/json/512"
done

grep -q "cc=${CC}" "${OUTPUT_DIR}/target.log" || {
  echo "upstream H3 ${CC} path was not established" >&2
  sed -n '1,240p' "${OUTPUT_DIR}/target.log" >&2
  exit 1
}

cat >"${OUTPUT_DIR}/workload.txt" <<EOF_WORKLOAD
scenario=upstream-h3-${CC}
round=${ROUND}
congestion_control=${CC}
small_requests=6000
medium_requests=3000
large_json_requests=1000
bulk_512k_requests=160
chunked_128k_requests=256
h3_downstream_json_requests=$((H3_JSON_PROBES * H3_JSON_REQUESTS))
h3_downstream_stream_requests=8
h3_downstream_bulk_requests=32
h3_downstream_compress_requests=32
resumption_cycles=8
resumption_requests=256
early_data_client=true
early_data_origin=true
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
