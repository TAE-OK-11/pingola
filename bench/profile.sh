#!/usr/bin/env bash
set -uo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
IMAGE=${PINGORA_IMAGE:-ghcr.io/tae-ok-11/pingora:local}
OUTPUT=${PROFILE_OUTPUT:-${ROOT}/bench/results/profile-$(date -u +%Y%m%dT%H%M%SZ)}
if [[ "${OUTPUT}" != /* ]]; then
  OUTPUT=${ROOT}/${OUTPUT}
fi
BACKEND_PORT=${PROFILE_BACKEND_PORT:-18800}
BACKEND_SOURCE=${PROFILE_BACKEND_SOURCE:-${ROOT}/bench/backend.rs}
PREBUILT_BACKEND=${PROFILE_BACKEND_BIN:-}
PERF_BIN=${PROFILE_PERF_BIN:-perf}
HTTP_PORT=${PROFILE_HTTP_PORT:-80}
HTTPS_PORT=${PROFILE_HTTPS_PORT:-443}
DURATION=${PROFILE_DURATION_SECONDS:-5}
CPUS=${PROFILE_CPUS:-}
MEMORY=${PROFILE_MEMORY:-}
NAME=pingora-profile-$$
BACKEND_PID=
DOCKER_RESOURCE_ARGS=()

if [[ -n "${CPUS}" ]]; then
  DOCKER_RESOURCE_ARGS+=(--cpus "${CPUS}")
fi
if [[ -n "${MEMORY}" ]]; then
  DOCKER_RESOURCE_ARGS+=(--memory "${MEMORY}")
fi

cleanup() {
  if docker inspect "${NAME}" >/dev/null 2>&1; then
    docker logs "${NAME}" >"${OUTPUT}/container.log" 2>&1 || true
    docker rm -f "${NAME}" >/dev/null 2>&1 || true
  fi
  if [[ -n "${BACKEND_PID}" ]]; then
    kill "${BACKEND_PID}" >/dev/null 2>&1 || true
    wait "${BACKEND_PID}" >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT INT TERM

mkdir -p "${OUTPUT}"
chmod 0755 "${OUTPUT}"
REQUIRED_COMMANDS=(docker curl wrk openssl strace)
if [[ -z "${PREBUILT_BACKEND}" ]]; then
  REQUIRED_COMMANDS+=(rustc)
fi
for command in "${REQUIRED_COMMANDS[@]}"; do
  if ! command -v "${command}" >/dev/null; then
    echo "missing required command: ${command}" >&2
    exit 2
  fi
done
if ! command -v "${PERF_BIN}" >/dev/null && [[ ! -x "${PERF_BIN}" ]]; then
  echo "missing perf executable: ${PERF_BIN}" >&2
  exit 2
fi

BACKEND_BIN=${OUTPUT}/backend-rust
if [[ -n "${PREBUILT_BACKEND}" ]]; then
  if [[ ! -x "${PREBUILT_BACKEND}" ]]; then
    echo "prebuilt profile backend is not executable: ${PREBUILT_BACKEND}" >&2
    exit 2
  fi
  install -m 0755 "${PREBUILT_BACKEND}" "${BACKEND_BIN}"
  printf 'prebuilt=%s\n' "${PREBUILT_BACKEND}" >"${OUTPUT}/backend-build.stdout"
  : >"${OUTPUT}/backend-build.stderr"
else
  if ! rustc --edition=2021 -D warnings -C opt-level=3 -C codegen-units=1 -C panic=abort \
    -C target-cpu=native -C strip=symbols \
    --remap-path-prefix="${ROOT}=." "${BACKEND_SOURCE}" -o "${BACKEND_BIN}" \
    >"${OUTPUT}/backend-build.stdout" 2>"${OUTPUT}/backend-build.stderr"; then
    echo "Rust profile backend build failed: source=${BACKEND_SOURCE} log=${OUTPUT}/backend-build.stderr" >&2
    sed -n '1,120p' "${OUTPUT}/backend-build.stderr" >&2
    exit 2
  fi
fi

openssl req -x509 -newkey rsa:2048 -nodes -days 1 \
  -subj '/CN=profile.test' -addext 'subjectAltName=DNS:profile.test' \
  -keyout "${OUTPUT}/key.pem" -out "${OUTPUT}/cert.pem" >/dev/null 2>&1
chown 0:10001 "${OUTPUT}/key.pem" "${OUTPUT}/cert.pem"
chmod 0640 "${OUTPUT}/key.pem" "${OUTPUT}/cert.pem"

cat >"${OUTPUT}/pingora.yaml" <<EOF
server:
  http_listen: ["127.0.0.1:${HTTP_PORT}"]
  https_listen: ["127.0.0.1:${HTTPS_PORT}"]
  certificate: /work/cert.pem
  private_key: /work/key.pem
  health_socket: /tmp/pingora/health.sock
  health_details: true
  threads: 1
  upstream_keepalive_pool_size: 128
  max_retries: 0
  access_log: false
  http2_max_concurrent_streams: 128
  graceful_shutdown_timeout_seconds: 2
trusted_proxies: ["127.0.0.0/8"]
upstreams:
  backend:
    address: "127.0.0.1:${BACKEND_PORT}"
hosts:
  profile:
    domains: ["profile.test"]
    handler: vaultwarden
    upstream: backend
route_limits:
  vaultwarden:
    rate_per_second: 0
    active_requests: 0
EOF
chmod 0644 "${OUTPUT}/pingora.yaml"

"${BACKEND_BIN}" --port "${BACKEND_PORT}" \
  >"${OUTPUT}/backend.stdout" 2>"${OUTPUT}/backend.stderr" &
BACKEND_PID=$!
BACKEND_READY=false
for _ in {1..100}; do
  if curl --noproxy '*' -fsS "http://127.0.0.1:${BACKEND_PORT}/bytes/64" \
    -o /dev/null 2>/dev/null; then
    BACKEND_READY=true
    break
  fi
  kill -0 "${BACKEND_PID}" 2>/dev/null || break
  sleep 0.05
done
if [[ "${BACKEND_READY}" != true ]]; then
  echo "profile backend failed readiness: port=${BACKEND_PORT} log=${OUTPUT}/backend.stderr" >&2
  exit 2
fi

if ! docker run --detach --name "${NAME}" --network host --read-only \
  "${DOCKER_RESOURCE_ARGS[@]}" \
  --cap-drop ALL --cap-add NET_BIND_SERVICE --security-opt no-new-privileges \
  --tmpfs /tmp/pingora:rw,noexec,nosuid,nodev,uid=10001,gid=10001,mode=0700 \
  --env PINGORA_ALLOCATOR_STATS=1 --volume "${OUTPUT}:/work:ro" \
  --entrypoint /usr/local/bin/pingora "${IMAGE}" --config /work/pingora.yaml >/dev/null; then
  echo "profile proxy failed to start: image=${IMAGE}" >&2
  exit 2
fi
PROXY_READY=false
for _ in {1..100}; do
  if curl --noproxy '*' -fsS -H 'host: profile.test' \
    "http://127.0.0.1:${HTTP_PORT}/bytes/64" -o /dev/null 2>/dev/null; then
    PROXY_READY=true
    break
  fi
  sleep 0.05
done
if [[ "${PROXY_READY}" != true ]]; then
  echo "profile proxy failed readiness: image=${IMAGE} port=${HTTP_PORT}" >&2
  exit 2
fi

PID=$(docker inspect --format '{{.State.Pid}}' "${NAME}")
cat >"${OUTPUT}/environment.txt" <<EOF
timestamp=$(date -u +%FT%TZ)
image=${IMAGE}
container_pid=${PID}
duration_seconds=${DURATION}
container_cpus=${CPUS:-unlimited}
container_memory=${MEMORY:-unlimited}
backend=rust-std-http1
backend_source=${BACKEND_SOURCE}
backend_prebuilt=${PREBUILT_BACKEND:-false}
backend_binary_sha256=$(sha256sum "${BACKEND_BIN}" | cut -d' ' -f1)
http_port=${HTTP_PORT}
https_port=${HTTPS_PORT}
backend_port=${BACKEND_PORT}
perf_event_paranoid=$(cat /proc/sys/kernel/perf_event_paranoid 2>/dev/null || echo unknown)
EOF
if [[ -z "${PREBUILT_BACKEND}" ]]; then
  rustc -vV | sed 's/^/backend_rustc_/' >>"${OUTPUT}/environment.txt"
fi
lscpu >>"${OUTPUT}/environment.txt" 2>&1

curl --noproxy '*' -fsS -H 'host: profile.test' \
  "http://127.0.0.1:${HTTP_PORT}/pingora-health/details?allocator=1" \
  >"${OUTPUT}/allocator-before.json" || true

wrk --latency -t1 -c8 -d "${DURATION}s" -s "${ROOT}/bench/wrk-keepalive.lua" \
  -H 'Host: profile.test' "http://127.0.0.1:${HTTP_PORT}/bytes/64" \
  >"${OUTPUT}/perf-stat-wrk.txt" 2>&1 &
LOAD_PID=$!
"${PERF_BIN}" stat -p "${PID}" \
  -e task-clock,cycles,instructions,branches,branch-misses,cache-references,cache-misses,context-switches,cpu-migrations,page-faults \
  -o "${OUTPUT}/perf-stat.txt" -- sleep "${DURATION}" \
  >"${OUTPUT}/perf-stat.stdout" 2>"${OUTPUT}/perf-stat.stderr"
PERF_STAT_RC=$?
wait "${LOAD_PID}" || true
echo "perf_stat_rc=${PERF_STAT_RC}" >>"${OUTPUT}/environment.txt"

wrk -t1 -c8 -d "${DURATION}s" -s "${ROOT}/bench/wrk-keepalive.lua" \
  -H 'Host: profile.test' "http://127.0.0.1:${HTTP_PORT}/bytes/64" \
  >"${OUTPUT}/perf-record-wrk.txt" 2>&1 &
LOAD_PID=$!
"${PERF_BIN}" record -F 99 -g --call-graph fp -p "${PID}" -o "${OUTPUT}/perf.data" \
  -- sleep "${DURATION}" >"${OUTPUT}/perf-record.stdout" 2>"${OUTPUT}/perf-record.stderr"
PERF_RECORD_RC=$?
wait "${LOAD_PID}" || true
echo "perf_record_rc=${PERF_RECORD_RC}" >>"${OUTPUT}/environment.txt"
if ((PERF_RECORD_RC == 0)); then
  timeout 20 env DEBUGINFOD_URLS= "${PERF_BIN}" report --stdio --percent-limit 0.5 \
    --sort comm,dso,symbol --no-children -i "${OUTPUT}/perf.data" \
    >"${OUTPUT}/perf-report.txt" 2>&1 || true
  timeout 20 env DEBUGINFOD_URLS= "${PERF_BIN}" script -i "${OUTPUT}/perf.data" \
    >"${OUTPUT}/perf-script.txt" 2>&1 || true
  if command -v stackcollapse-perf.pl >/dev/null && command -v flamegraph.pl >/dev/null; then
    stackcollapse-perf.pl "${OUTPUT}/perf-script.txt" >"${OUTPUT}/perf.folded"
    flamegraph.pl "${OUTPUT}/perf.folded" >"${OUTPUT}/flamegraph.svg"
  else
    echo "FlameGraph scripts unavailable; perf.data and perf-script.txt were preserved" \
      >"${OUTPUT}/flamegraph-unavailable.txt"
  fi
fi

timeout --signal=INT "$((DURATION + 1))" strace -f -c -p "${PID}" \
  -o "${OUTPUT}/strace-summary.txt" >"${OUTPUT}/strace.stdout" \
  2>"${OUTPUT}/strace.stderr" &
STRACE_PID=$!
sleep 0.2
wrk -t1 -c8 -d "${DURATION}s" -s "${ROOT}/bench/wrk-keepalive.lua" \
  -H 'Host: profile.test' "http://127.0.0.1:${HTTP_PORT}/bytes/64" \
  >"${OUTPUT}/strace-wrk.txt" 2>&1 || true
wait "${STRACE_PID}"
STRACE_RC=$?
echo "strace_rc=${STRACE_RC}" >>"${OUTPUT}/environment.txt"

curl --noproxy '*' -fsS -H 'host: profile.test' \
  "http://127.0.0.1:${HTTP_PORT}/pingora-health/details?allocator=1" \
  >"${OUTPUT}/allocator-after.json" || true

echo "profile results=${OUTPUT} perf_stat_rc=${PERF_STAT_RC} perf_record_rc=${PERF_RECORD_RC} strace_rc=${STRACE_RC}"
