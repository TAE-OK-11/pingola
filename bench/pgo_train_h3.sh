#!/usr/bin/env bash
set -euo pipefail

# shellcheck source=bench/pgo_train_scale.sh
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/pgo_train_scale.sh"

PINGORA_BIN=${1:?usage: pgo_train_h3.sh PINGORA_BIN BACKEND_BIN HTTP3_PROBE_BIN OUTPUT_DIR}
BACKEND_BIN=${2:?usage: pgo_train_h3.sh PINGORA_BIN BACKEND_BIN HTTP3_PROBE_BIN OUTPUT_DIR}
HTTP3_PROBE_BIN=${3:?usage: pgo_train_h3.sh PINGORA_BIN BACKEND_BIN HTTP3_PROBE_BIN OUTPUT_DIR}
OUTPUT_DIR=${4:?usage: pgo_train_h3.sh PINGORA_BIN BACKEND_BIN HTTP3_PROBE_BIN OUTPUT_DIR}

ECDSA_CURVE=${PGO_ECDSA_CURVE:-prime256v1}
REQUIRE_PROFILE=${PGO_REQUIRE_PROFILE:-true}
HTTP3_PORT=${PGO_HTTP3_PORT:-19443}
BACKEND_PORT=${PGO_BACKEND_PORT:-19000}
ROUND=${PGO_TRAIN_ROUND:-1}
RUNTIME_DIR=${OUTPUT_DIR}/runtime
STATIC_DIR=${RUNTIME_DIR}/static
BACKEND_PID=
PINGORA_PID=

case "${ECDSA_CURVE}" in
  prime256v1|secp384r1) ;;
  *)
    echo "unsupported ECDSA curve: ${ECDSA_CURVE}" >&2
    exit 2
    ;;
esac

for binary in "${PINGORA_BIN}" "${BACKEND_BIN}" "${HTTP3_PROBE_BIN}"; do
  if [[ ! -x "${binary}" ]]; then
    echo "required PGO binary is not executable: ${binary}" >&2
    exit 2
  fi
done

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

wait_tcp() {
  local port=$1
  local process_pid=$2
  local name=$3

  for _ in {1..160}; do
    if (exec 3<>"/dev/tcp/127.0.0.1/${port}") 2>/dev/null; then
      exec 3>&-
      return 0
    fi
    if ! kill -0 "${process_pid}" 2>/dev/null; then
      echo "${name} exited before readiness" >&2
      return 1
    fi
    sleep 0.05
  done

  echo "${name} did not become ready on 127.0.0.1:${port}" >&2
  return 1
}

run_probe() {
  local name=$1
  local authority=$2
  local path=$3
  local requests=$4
  local accept_encoding=${5:-}
  local -a probe_args=("127.0.0.1:${HTTP3_PORT}" "${authority}" "${path}" "${requests}")

  if [[ -n "${accept_encoding}" ]]; then
    probe_args+=("${accept_encoding}")
  fi

  if ! "${HTTP3_PROBE_BIN}" "${probe_args[@]}" \
    >"${OUTPUT_DIR}/${name}.out" 2>"${OUTPUT_DIR}/${name}.log"; then
    echo "HTTP/3 PGO workload failed: ${name}" >&2
    sed -n '1,160p' "${OUTPUT_DIR}/${name}.log" >&2
    return 1
  fi
}

rm -rf "${RUNTIME_DIR}"
install -d -m 0700 "${OUTPUT_DIR}" "${RUNTIME_DIR}" "${STATIC_DIR}"

openssl genpkey -algorithm EC -pkeyopt "ec_paramgen_curve:${ECDSA_CURVE}" \
  -out "${RUNTIME_DIR}/key.pem" >"${OUTPUT_DIR}/openssl-key.log" 2>&1
openssl req -new -x509 -sha256 -days 1 -key "${RUNTIME_DIR}/key.pem" \
  -subj '/CN=pgo.test' \
  -addext 'subjectAltName=DNS:pgo.test,DNS:static.test,DNS:music.test,DNS:couch.test,DNS:cdn.test' \
  -out "${RUNTIME_DIR}/cert.pem" >"${OUTPUT_DIR}/openssl-cert.log" 2>&1
chmod 0600 "${RUNTIME_DIR}/key.pem" "${RUNTIME_DIR}/cert.pem"
openssl pkey -in "${RUNTIME_DIR}/key.pem" -check -noout \
  >>"${OUTPUT_DIR}/openssl-key.log" 2>&1

dd if=/dev/zero of="${STATIC_DIR}/hot.bin" bs=4096 count=1 status=none
printf '%8192s' | tr ' ' 'x' >"${STATIC_DIR}/train.json"

cat >"${OUTPUT_DIR}/pingora.yaml" <<EOF_YAML
server:
  http_listen: []
  https_listen: []
  http3_listen: ["127.0.0.1:${HTTP3_PORT}"]
  http3_max_idle_timeout_seconds: 60
  http3_max_concurrent_streams: 64
  # Fresh-handshake probes remain active until the QUIC idle timeout expires.
  # Leave room for readiness and concurrent probes above the production cap.
  http3_max_connections_per_ip: 128
  certificate: ${RUNTIME_DIR}/cert.pem
  private_key: ${RUNTIME_DIR}/key.pem
  health_socket: ${RUNTIME_DIR}/health.sock
  threads: 1
  upstream_keepalive_pool_size: 128
  # Persistent profile probes deliberately exceed the production value (500).
  downstream_keepalive_requests: 1000000
  # Concurrent probes send 512 requests per QUIC connection; keep above that cap.
  http3_max_requests_per_connection: 1000000
  downstream_max_connections: 4096
  max_retries: 2
  access_log: false
  static_cache_bytes: 1048576
  # Keep profile collection focused on proxy work rather than admission control.
  static_active_requests_per_client: 1000000
  graceful_shutdown_timeout_seconds: 2
trusted_proxies: ["127.0.0.0/8"]
upstreams:
  backend:
    address: "127.0.0.1:${BACKEND_PORT}"
    protocol: http1
    connect_timeout_seconds: 2
    read_timeout_seconds: 60
    write_timeout_seconds: 60
    idle_timeout_seconds: 30
hosts:
  api: { domains: ["pgo.test"], handler: vaultwarden, upstream: backend }
  music: { domains: ["music.test"], handler: navidrome-main, upstream: backend }
  static: { domains: ["static.test"], handler: static, static_root: ${STATIC_DIR} }
  couch: { domains: ["couch.test"], handler: couchdb, upstream: backend }
  cdn: { domains: ["cdn.test"], handler: navidrome-cdn, upstream: backend }
route_limits:
  vaultwarden: { rate_per_second: 0, active_requests: 0 }
  navidrome_api: { rate_per_second: 0, active_requests: 0 }
  navidrome_stream: { rate_per_second: 0, active_requests: 0 }
  navidrome_cover: { rate_per_second: 0, active_requests: 0 }
  couchdb: { rate_per_second: 0, active_requests: 0 }
EOF_YAML

"${BACKEND_BIN}" --port "${BACKEND_PORT}" >"${OUTPUT_DIR}/backend.log" 2>&1 &
BACKEND_PID=$!
wait_tcp "${BACKEND_PORT}" "${BACKEND_PID}" backend

CHECK_PATTERN="${RUNTIME_DIR}/check-%p-%m.profraw"
if ! LLVM_PROFILE_FILE="${CHECK_PATTERN}" \
  "${PINGORA_BIN}" --config "${OUTPUT_DIR}/pingora.yaml" --check \
  >"${OUTPUT_DIR}/pingora-check.log" 2>&1; then
  echo "HTTP/3 PGO configuration preflight failed: round=${ROUND}" >&2
  sed -n '1,240p' "${OUTPUT_DIR}/pingora-check.log" >&2
  exit 1
fi
rm -f "${RUNTIME_DIR}"/check-*.profraw

LLVM_PROFILE_FILE="${OUTPUT_DIR}/pingora-h3-r${ROUND}-%p-%m.profraw" \
  "${PINGORA_BIN}" --config "${OUTPUT_DIR}/pingora.yaml" \
  >"${OUTPUT_DIR}/pingora.log" 2>&1 &
PINGORA_PID=$!

ready=false
for _ in {1..160}; do
  if "${HTTP3_PROBE_BIN}" "127.0.0.1:${HTTP3_PORT}" pgo.test /json/512 \
    >"${OUTPUT_DIR}/readiness.out" 2>"${OUTPUT_DIR}/readiness.log"; then
    ready=true
    break
  fi
  if ! kill -0 "${PINGORA_PID}" 2>/dev/null; then
    echo "Pingora exited before HTTP/3 readiness" >&2
    sed -n '1,240p' "${OUTPUT_DIR}/pingora.log" >&2
    exit 1
  fi
  sleep 0.05
done
if [[ "${ready}" != true ]]; then
  echo "HTTP/3 listener did not become ready on 127.0.0.1:${HTTP3_PORT}" >&2
  sed -n '1,240p' "${OUTPUT_DIR}/pingora.log" >&2
  sed -n '1,120p' "${OUTPUT_DIR}/readiness.log" >&2
  exit 1
fi

# Train fresh QUIC handshakes and stateless Retry without allowing this path to
# dominate the profile. Persistent connections below carry the bulk workload.
for index in $(seq 1 "$(pgo_train_scale 64)"); do
  run_probe "h3-handshake-${index}" pgo.test /json/512 1
done

# Eight concurrent long-lived QUIC connections exercise multiplexed request
# dispatch, QPACK, task scheduling, loopback proxying, and response framing.
probe_pids=()
for index in $(seq 1 8); do
  run_probe "h3-json-concurrent-${index}" pgo.test /json/512 "$(pgo_train_scale 512)" &
  probe_pids+=("$!")
done
for pid in "${probe_pids[@]}"; do
  wait "${pid}"
done

run_probe h3-static static.test /hot.bin "$(pgo_train_scale 2000)"
run_probe h3-large-json couch.test /json/65536 "$(pgo_train_scale 256)"
run_probe h3-compress-json pgo.test /json/65536 "$(pgo_train_scale 128)" "zstd"
run_probe h3-compress-couch couch.test /json/65536 "$(pgo_train_scale 64)" "br, zstd, gzip"
run_probe h3-compress-static static.test /train.json "$(pgo_train_scale 128)" "zstd"
run_probe h3-medium-body cdn.test /bytes/4096 "$(pgo_train_scale 1000)"
run_probe h3-music-stream music.test /stream/1048576 "$(pgo_train_scale 16)"

upstream_connections=$(curl --noproxy '*' --fail --silent --show-error \
  "http://127.0.0.1:${BACKEND_PORT}/stats/connections")
if [[ ! "${upstream_connections}" =~ ^[0-9]+$ ]]; then
  echo "invalid backend connection count: ${upstream_connections}" >&2
  exit 1
fi

cat >"${OUTPUT_DIR}/workload.txt" <<EOF_WORKLOAD
scenario=h3
round=${ROUND}
ecdsa_curve=${ECDSA_CURVE}
http3_port=${HTTP3_PORT}
backend_port=${BACKEND_PORT}
backend_connections=${upstream_connections}
fresh_handshakes=64
concurrent_connections=8
persistent_requests=7368
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
