#!/usr/bin/env bash
set -euo pipefail

# shellcheck source=bench/pgo_train_scale.sh
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/pgo_train_scale.sh"

PINGORA_BIN=${1:?usage: pgo_train_grpc.sh PINGORA_BIN GRPC_ORIGIN_BIN CLIENT_BIN OUTPUT_DIR}
GRPC_ORIGIN_BIN=${2:?usage: pgo_train_grpc.sh PINGORA_BIN GRPC_ORIGIN_BIN CLIENT_BIN OUTPUT_DIR}
CLIENT_BIN=${3:?usage: pgo_train_grpc.sh PINGORA_BIN GRPC_ORIGIN_BIN CLIENT_BIN OUTPUT_DIR}
OUTPUT_DIR=${4:?usage: pgo_train_grpc.sh PINGORA_BIN GRPC_ORIGIN_BIN CLIENT_BIN OUTPUT_DIR}

ECDSA_CURVE=${PGO_ECDSA_CURVE:-prime256v1}
REQUIRE_PROFILE=${PGO_REQUIRE_PROFILE:-true}
HTTP_PORT=${PGO_GRPC_HTTP_PORT:-19460}
HTTPS_PORT=${PGO_GRPC_HTTPS_PORT:-19461}
ORIGIN_PORT=${PGO_GRPC_ORIGIN_PORT:-19051}
ROUND=${PGO_TRAIN_ROUND:-1}
RUNTIME_DIR=${OUTPUT_DIR}/runtime
GRPC_ORIGIN_PID=
PINGORA_PID=

case "${ECDSA_CURVE}" in
  prime256v1|secp384r1) ;;
  *)
    echo "unsupported ECDSA curve: ${ECDSA_CURVE}" >&2
    exit 2
    ;;
esac

for binary in "${PINGORA_BIN}" "${GRPC_ORIGIN_BIN}" "${CLIENT_BIN}"; do
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
  if [[ -n "${GRPC_ORIGIN_PID}" ]]; then
    kill -TERM "${GRPC_ORIGIN_PID}" >/dev/null 2>&1 || true
    wait "${GRPC_ORIGIN_PID}" >/dev/null 2>&1 || true
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

run_h2load() {
  local name=$1
  shift

  h2load "$@" >"${OUTPUT_DIR}/${name}.log" 2>&1
  if ! grep -Eq '0 failed, 0 errored' "${OUTPUT_DIR}/${name}.log"; then
    echo "h2load gRPC workload failed: ${name}" >&2
    sed -n '1,180p' "${OUTPUT_DIR}/${name}.log" >&2
    exit 1
  fi
}

rm -rf "${RUNTIME_DIR}"
install -d -m 0700 "${OUTPUT_DIR}" "${RUNTIME_DIR}"

openssl genpkey -algorithm EC -pkeyopt "ec_paramgen_curve:${ECDSA_CURVE}" \
  -out "${RUNTIME_DIR}/key.pem" >"${OUTPUT_DIR}/openssl-key.log" 2>&1
openssl req -new -x509 -sha256 -days 1 -key "${RUNTIME_DIR}/key.pem" \
  -subj '/CN=music.test' \
  -addext 'subjectAltName=DNS:music.test,DNS:cdn.test' \
  -out "${RUNTIME_DIR}/cert.pem" >"${OUTPUT_DIR}/openssl-cert.log" 2>&1
chmod 0600 "${RUNTIME_DIR}/key.pem" "${RUNTIME_DIR}/cert.pem"

printf '\x00\x00\x00\x00\x00' >"${RUNTIME_DIR}/empty.grpc"

cat >"${OUTPUT_DIR}/pingora.yaml" <<EOF_YAML
server:
  http_listen: ["127.0.0.1:${HTTP_PORT}"]
  https_listen: ["127.0.0.1:${HTTPS_PORT}"]
  certificate: ${RUNTIME_DIR}/cert.pem
  private_key: ${RUNTIME_DIR}/key.pem
  health_socket: ${RUNTIME_DIR}/health.sock
  threads: 1
  upstream_keepalive_pool_size: 32
  downstream_keepalive_requests: 500
  max_retries: 0
  access_log: false
  http2_max_concurrent_streams: 32
  graceful_shutdown_timeout_seconds: 2
trusted_proxies: ["127.0.0.0/8"]
upstreams:
  navidrome:
    address: "127.0.0.1:${ORIGIN_PORT}"
    protocol: http1
  navidrome_grpc:
    address: "127.0.0.1:${ORIGIN_PORT}"
    protocol: grpc
    http2_max_concurrent_streams: 128
    connect_timeout_seconds: 2
    read_timeout_seconds: 30
    write_timeout_seconds: 30
    idle_timeout_seconds: 30
    warmup_on_start: true
    warmup_path: /navidrome.Subsonic/Ping
hosts:
  music: { domains: ["music.test"], handler: navidrome-main, upstream: navidrome }
  cdn: { domains: ["cdn.test"], handler: navidrome-cdn, upstream: navidrome }
route_limits:
  navidrome_api: { rate_per_second: 0, active_requests: 0 }
  navidrome_stream: { rate_per_second: 0, active_requests: 0 }
  navidrome_cover: { rate_per_second: 0, active_requests: 0 }
  navidrome_grpc: { rate_per_second: 0, active_requests: 0 }
EOF_YAML

"${GRPC_ORIGIN_BIN}" "127.0.0.1:${ORIGIN_PORT}" >"${OUTPUT_DIR}/origin.log" 2>&1 &
GRPC_ORIGIN_PID=$!
wait_tcp "${ORIGIN_PORT}" "${GRPC_ORIGIN_PID}" grpc-origin

CHECK_PATTERN="${RUNTIME_DIR}/check-%p-%m.profraw"
if ! LLVM_PROFILE_FILE="${CHECK_PATTERN}" \
  "${PINGORA_BIN}" --config "${OUTPUT_DIR}/pingora.yaml" --check \
  >"${OUTPUT_DIR}/pingora-check.log" 2>&1; then
  echo "gRPC PGO configuration preflight failed: round=${ROUND}" >&2
  sed -n '1,240p' "${OUTPUT_DIR}/pingora-check.log" >&2
  exit 1
fi
rm -f "${RUNTIME_DIR}"/check-*.profraw

LLVM_PROFILE_FILE="${OUTPUT_DIR}/pingora-grpc-r${ROUND}-%p-%m.profraw" \
  "${PINGORA_BIN}" --config "${OUTPUT_DIR}/pingora.yaml" \
  >"${OUTPUT_DIR}/pingora.log" 2>&1 &
PINGORA_PID=$!
if ! wait_tcp "${HTTP_PORT}" "${PINGORA_PID}" Pingora; then
  sed -n '1,200p' "${OUTPUT_DIR}/pingora.log" >&2
  exit 1
fi

run_h2load grpc-h2-unary --h2 -n "$(pgo_train_scale 8000)" -c 4 -m 16 -d "${RUNTIME_DIR}/empty.grpc" \
  --sni music.test \
  -H 'host: music.test' \
  -H 'content-type: application/grpc' \
  -H 'te: trailers' \
  "https://127.0.0.1:${HTTPS_PORT}/navidrome.Subsonic/Ping"

run_h2load grpc-h2-proto --h2 -n "$(pgo_train_scale 4000)" -c 4 -m 16 -d "${RUNTIME_DIR}/empty.grpc" \
  --sni music.test \
  -H 'host: music.test' \
  -H 'content-type: application/grpc+proto' \
  -H 'te: trailers' \
  "https://127.0.0.1:${HTTPS_PORT}/navidrome.Subsonic/GetAlbum"

run_h2load grpc-h2-cdn --h2 -n "$(pgo_train_scale 2000)" -c 2 -m 8 -d "${RUNTIME_DIR}/empty.grpc" \
  --sni cdn.test \
  -H 'host: cdn.test' \
  -H 'content-type: application/grpc' \
  -H 'te: trailers' \
  "https://127.0.0.1:${HTTPS_PORT}/navidrome.Subsonic/Ping"

for _ in $(seq 1 "$(pgo_train_scale 400)"); do
  curl --noproxy '*' --http1.1 --insecure --fail --silent --show-error --output /dev/null \
    --resolve "music.test:${HTTPS_PORT}:127.0.0.1" \
    -H 'content-type: application/grpc-web+proto' \
    -H 'te: trailers' \
    --data-binary @"${RUNTIME_DIR}/empty.grpc" \
    "https://music.test:${HTTPS_PORT}/navidrome.Subsonic/Ping"
done

for _ in $(seq 1 "$(pgo_train_scale 200)"); do
  curl --noproxy '*' --http2 --insecure --fail --silent --show-error --output /dev/null \
    --resolve "music.test:${HTTPS_PORT}:127.0.0.1" \
    -H 'content-type: application/grpc-web' \
    -H 'te: trailers' \
    --data-binary @"${RUNTIME_DIR}/empty.grpc" \
    "https://music.test:${HTTPS_PORT}/navidrome.Subsonic/Ping"
done

# Cleartext h2c native gRPC through the HTTP listener trains the same
# classify/TE/empty-DATA path without TLS.
for _ in $(seq 1 "$(pgo_train_scale 300)"); do
  curl --noproxy '*' --http1.1 --fail --silent --show-error --output /dev/null \
    -H 'Host: music.test' \
    -H 'content-type: application/grpc' \
    -H 'te: trailers' \
    --data-binary @"${RUNTIME_DIR}/empty.grpc" \
    "http://127.0.0.1:${HTTP_PORT}/navidrome.Subsonic/Ping"
done

kill -TERM "${PINGORA_PID}"
wait "${PINGORA_PID}"
PINGORA_PID=

if [[ "${REQUIRE_PROFILE}" == true ]]; then
  compgen -G "${OUTPUT_DIR}/*.profraw" >/dev/null
elif [[ "${REQUIRE_PROFILE}" != false ]]; then
  echo 'PGO_REQUIRE_PROFILE must be true or false' >&2
  exit 2
fi

cat >"${OUTPUT_DIR}/workload.txt" <<EOF_WORKLOAD
scenario=grpc
round=${ROUND}
ecdsa_curve=${ECDSA_CURVE}
http_port=${HTTP_PORT}
https_port=${HTTPS_PORT}
origin_port=${ORIGIN_PORT}
EOF_WORKLOAD
