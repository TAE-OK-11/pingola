#!/usr/bin/env bash
set -uo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
IMAGE=${PINGORA_IMAGE:-ghcr.io/tae-ok-11/pingora:local}
OUTPUT=${PROFILE_OUTPUT:-${ROOT}/bench/results/profile-$(date -u +%Y%m%dT%H%M%SZ)}
BACKEND_PORT=${PROFILE_BACKEND_PORT:-18800}
HTTP_PORT=${PROFILE_HTTP_PORT:-18880}
HTTPS_PORT=${PROFILE_HTTPS_PORT:-18843}
DURATION=${PROFILE_DURATION_SECONDS:-5}
NAME=pingora-profile-$$
BACKEND_PID=

cleanup() {
  docker logs "${NAME}" >"${OUTPUT}/container.log" 2>&1 || true
  docker rm -f "${NAME}" >/dev/null 2>&1 || true
  if [[ -n "${BACKEND_PID}" ]]; then
    kill "${BACKEND_PID}" >/dev/null 2>&1 || true
    wait "${BACKEND_PID}" >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT INT TERM

mkdir -p "${OUTPUT}"
chmod 0755 "${OUTPUT}"
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

python3 "${ROOT}/bench/backend.py" --port "${BACKEND_PORT}" \
  >"${OUTPUT}/backend.stdout" 2>"${OUTPUT}/backend.stderr" &
BACKEND_PID=$!
for _ in {1..100}; do
  curl --noproxy '*' -fsS "http://127.0.0.1:${BACKEND_PORT}/bytes/64" -o /dev/null && break
  sleep 0.05
done

docker run --detach --name "${NAME}" --network host --read-only \
  --cap-drop ALL --cap-add NET_BIND_SERVICE --security-opt no-new-privileges \
  --tmpfs /tmp/pingora:rw,noexec,nosuid,nodev,uid=10001,gid=10001,mode=0700 \
  --env PINGORA_ALLOCATOR_STATS=1 --volume "${OUTPUT}:/work:ro" \
  --entrypoint /usr/local/bin/pingora "${IMAGE}" --config /work/pingora.yaml >/dev/null
for _ in {1..100}; do
  if curl --noproxy '*' -fsS -H 'host: profile.test' \
    "http://127.0.0.1:${HTTP_PORT}/bytes/64" -o /dev/null 2>/dev/null; then
    break
  fi
  sleep 0.05
done

PID=$(docker inspect --format '{{.State.Pid}}' "${NAME}")
cat >"${OUTPUT}/environment.txt" <<EOF
timestamp=$(date -u +%FT%TZ)
image=${IMAGE}
container_pid=${PID}
duration_seconds=${DURATION}
perf_event_paranoid=$(cat /proc/sys/kernel/perf_event_paranoid 2>/dev/null || echo unknown)
EOF
lscpu >>"${OUTPUT}/environment.txt" 2>&1

curl --noproxy '*' -fsS -H 'host: profile.test' \
  "http://127.0.0.1:${HTTP_PORT}/pingora-health/details?allocator=1" \
  >"${OUTPUT}/allocator-before.json" || true

wrk --latency -t1 -c8 -d "${DURATION}s" -s "${ROOT}/bench/wrk-keepalive.lua" \
  -H 'Host: profile.test' "http://127.0.0.1:${HTTP_PORT}/bytes/64" \
  >"${OUTPUT}/perf-stat-wrk.txt" 2>&1 &
LOAD_PID=$!
perf stat -p "${PID}" \
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
perf record -F 99 -g --call-graph fp -p "${PID}" -o "${OUTPUT}/perf.data" \
  -- sleep "${DURATION}" >"${OUTPUT}/perf-record.stdout" 2>"${OUTPUT}/perf-record.stderr"
PERF_RECORD_RC=$?
wait "${LOAD_PID}" || true
echo "perf_record_rc=${PERF_RECORD_RC}" >>"${OUTPUT}/environment.txt"
if ((PERF_RECORD_RC == 0)); then
  timeout 20 env DEBUGINFOD_URLS= perf report --stdio --percent-limit 0.5 \
    --sort comm,dso,symbol --no-children -i "${OUTPUT}/perf.data" \
    >"${OUTPUT}/perf-report.txt" 2>&1 || true
  timeout 20 env DEBUGINFOD_URLS= perf script -i "${OUTPUT}/perf.data" \
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
