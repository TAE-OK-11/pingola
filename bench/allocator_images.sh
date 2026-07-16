#!/usr/bin/env bash
set -uo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
JEMALLOC_IMAGE=${JEMALLOC_IMAGE:-ghcr.io/tae-ok-11/pingora@sha256:78ccd4006270d6ccc98be36a6d912a7bd0a16108e87f6aa4af56a57c7da8cff5}
TCMALLOC_IMAGE=${TCMALLOC_IMAGE:-ghcr.io/tae-ok-11/pingora@sha256:46244afee72d96cc2aa465416358febdfbbeadd086a875a1cb032ce163c35b44}
JEMALLOC_EXPECTED_ALLOCATOR=${ALLOCATOR_BENCH_JEMALLOC_EXPECTED:-jemalloc}
TCMALLOC_EXPECTED_ALLOCATOR=${ALLOCATOR_BENCH_TCMALLOC_EXPECTED:-tcmalloc}
BACKEND_IMAGE=${ALLOCATOR_BACKEND_IMAGE:-tae00217/jbs-nginx:ultra-4.0}
CPU_LIMIT=${ALLOCATOR_BENCH_CPUS:-0.5}
MEMORY_LIMIT=${ALLOCATOR_BENCH_MEMORY:-1g}
MIN_FREE_BYTES=${ALLOCATOR_BENCH_MIN_FREE_BYTES:-1073741824}
ROUNDS=${ALLOCATOR_BENCH_ROUNDS:-5}
DURATION=${ALLOCATOR_BENCH_DURATION:-3s}
WARMUP=${ALLOCATOR_BENCH_WARMUP:-1s}
PROFILE=${ALLOCATOR_BENCH_PROFILE:-standard}
BACKEND_PORT=${ALLOCATOR_BACKEND_PORT:-18900}
HTTP_PORT=${ALLOCATOR_HTTP_PORT:-18980}
HTTPS_PORT=${ALLOCATOR_HTTPS_PORT:-18943}
OUTPUT=${ALLOCATOR_BENCH_OUTPUT:-${ROOT}/bench/results/allocator-images-$(date -u +%Y%m%dT%H%M%SZ)}
if [[ "${OUTPUT}" != /* ]]; then
  OUTPUT=${ROOT}/${OUTPUT}
fi
NAME=pingora-allocator-bench-$$
BACKEND_NAME=${NAME}-backend
case "${PROFILE}" in
  smoke)
    PROTOCOLS=(h1-keepalive h2-single)
    PAYLOADS=(64)
    CONCURRENCIES=(1)
    ;;
  standard)
    PROTOCOLS=(h1-keepalive h2-single h2-multi)
    PAYLOADS=(64 4096)
    CONCURRENCIES=(1 8 32)
    ;;
  tls)
    PROTOCOLS=(h1-tls-keepalive h1-new-tls h2-single h2-multi)
    PAYLOADS=(64 4096)
    CONCURRENCIES=(1 8 32)
    ;;
  *)
    echo "ALLOCATOR_BENCH_PROFILE must be smoke, standard, or tls" >&2
    exit 2
    ;;
esac
PROXY_NAME=
FAILURES=0

cleanup() {
  if [[ -n "${PROXY_NAME}" ]]; then
    docker logs "${PROXY_NAME}" >"${OUTPUT}/raw/${PROXY_NAME}.final.log" 2>&1 || true
    docker rm -f "${PROXY_NAME}" >/dev/null 2>&1 || true
  fi
  docker logs "${BACKEND_NAME}" >"${OUTPUT}/raw/${BACKEND_NAME}.final.log" 2>&1 || true
  docker rm -f "${BACKEND_NAME}" >/dev/null 2>&1 || true
}
handle_signal() {
  trap - EXIT INT TERM
  cleanup
  exit 130
}
trap cleanup EXIT
trap handle_signal INT TERM

for command in docker curl wrk h2load openssl python3 sha256sum jq numfmt dd; do
  if ! command -v "${command}" >/dev/null; then
    echo "missing required command: ${command}" >&2
    exit 2
  fi
done

mkdir -p "${OUTPUT}/raw"
chmod 0755 "${OUTPUT}" "${OUTPUT}/raw"
AVAILABLE_BYTES=$(df --output=avail -B1 "${OUTPUT}" | tail -1 | tr -d ' ')
if ((AVAILABLE_BYTES < MIN_FREE_BYTES)); then
  echo "insufficient benchmark disk space: available=${AVAILABLE_BYTES} required=${MIN_FREE_BYTES} path=${OUTPUT}" >&2
  exit 2
fi

CPU_NANO=$(python3 - "${CPU_LIMIT}" <<'PY'
import sys
print(int(float(sys.argv[1]) * 1_000_000_000))
PY
)
MEMORY_BYTES=$(numfmt --from=iec "${MEMORY_LIMIT^^}")

cat >"${OUTPUT}/environment.txt" <<EOF
timestamp=$(date -u +%FT%TZ)
jemalloc_image=${JEMALLOC_IMAGE}
tcmalloc_image=${TCMALLOC_IMAGE}
jemalloc_expected_allocator=${JEMALLOC_EXPECTED_ALLOCATOR}
tcmalloc_expected_allocator=${TCMALLOC_EXPECTED_ALLOCATOR}
cpu_limit=${CPU_LIMIT}
cpu_nano=${CPU_NANO}
memory_limit=${MEMORY_LIMIT}
memory_bytes=${MEMORY_BYTES}
minimum_free_bytes=${MIN_FREE_BYTES}
available_bytes_at_start=${AVAILABLE_BYTES}
rounds=${ROUNDS}
profile=${PROFILE}
duration=${DURATION}
warmup=${WARMUP}
note=load generator and synthetic backend are unbounded host processes; only each proxy container is limited
backend_image=${BACKEND_IMAGE}
EOF
lscpu >>"${OUTPUT}/environment.txt" 2>&1
docker version >>"${OUTPUT}/environment.txt" 2>&1

for allocator in jemalloc tcmalloc; do
  if [[ "${allocator}" == jemalloc ]]; then
    image=${JEMALLOC_IMAGE}
    expected_allocator=${JEMALLOC_EXPECTED_ALLOCATOR}
  else
    image=${TCMALLOC_IMAGE}
    expected_allocator=${TCMALLOC_EXPECTED_ALLOCATOR}
  fi
  docker image inspect "${image}" >"${OUTPUT}/${allocator}-inspect.json"
  docker run --rm --entrypoint /usr/local/bin/pingora "${image}" --allocator-info \
    >"${OUTPUT}/${allocator}-allocator.txt"
  grep -q "^allocator=${expected_allocator} " "${OUTPUT}/${allocator}-allocator.txt"
done
docker image inspect "${BACKEND_IMAGE}" >"${OUTPUT}/backend-inspect.json"

openssl req -x509 -newkey rsa:2048 -nodes -days 1 \
  -subj '/CN=bench.test' -addext 'subjectAltName=DNS:bench.test' \
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
  threads: 1
  upstream_keepalive_pool_size: 128
  max_retries: 0
  access_log: false
  health_details: false
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
  bench:
    domains: ["bench.test"]
    handler: vaultwarden
    upstream: backend
route_limits:
  vaultwarden:
    rate_per_second: 0
    active_requests: 0
EOF
chmod 0644 "${OUTPUT}/pingora.yaml"

dd if=/dev/zero of="${OUTPUT}/payload-64.bin" bs=64 count=1 status=none
dd if=/dev/zero of="${OUTPUT}/payload-4096.bin" bs=4096 count=1 status=none
chmod 0644 "${OUTPUT}/payload-64.bin" "${OUTPUT}/payload-4096.bin"
cat >"${OUTPUT}/backend-nginx.conf" <<EOF
user nginx;
worker_processes 1;
pid /tmp/allocator-bench-nginx.pid;
error_log /dev/stderr warn;
events { worker_connections 4096; }
http {
  access_log off;
  sendfile on;
  tcp_nodelay on;
  keepalive_timeout 30s;
  keepalive_requests 10000;
  server {
    listen 127.0.0.1:${BACKEND_PORT};
    server_name _;
    location = /bytes/64 {
      alias /work/payload-64.bin;
      default_type application/octet-stream;
    }
    location = /bytes/4096 {
      alias /work/payload-4096.bin;
      default_type application/octet-stream;
    }
  }
}
EOF
chmod 0644 "${OUTPUT}/backend-nginx.conf"
if ! docker run --detach --name "${BACKEND_NAME}" --network host \
  --ulimit nofile=32768:32768 \
  --volume "${OUTPUT}:/work:ro" --entrypoint /usr/sbin/nginx "${BACKEND_IMAGE}" \
  -c /work/backend-nginx.conf -g 'daemon off;' >/dev/null; then
  echo "benchmark backend failed to start" >&2
  exit 1
fi
backend_ready=false
for _ in {1..100}; do
  if curl --noproxy '*' -fsS "http://127.0.0.1:${BACKEND_PORT}/bytes/64" -o /dev/null; then
    backend_ready=true
    break
  fi
  sleep 0.05
done
if [[ "${backend_ready}" != true ]]; then
  echo "benchmark backend did not become ready at 127.0.0.1:${BACKEND_PORT}" >&2
  docker logs "${BACKEND_NAME}" >&2 || true
  exit 1
fi

printf 'allocator\tprotocol\tpayload_bytes\tconcurrency\tround\tstatus\trps\tp50_us\tp90_us\tp95_us\tp99_us\tp999_us\tmax_us\tcpu_avg_pct\tcpu_peak_pct\trss_avg_kib\trss_peak_kib\terrors\tstatus_distribution\traw\n' \
  >"${OUTPUT}/results.tsv"

stop_proxy() {
  if [[ -n "${PROXY_NAME}" ]]; then
    docker logs "${PROXY_NAME}" >"${OUTPUT}/raw/${PROXY_NAME}.log" 2>&1 || true
    docker rm -f "${PROXY_NAME}" >/dev/null 2>&1 || true
    PROXY_NAME=
  fi
}

start_proxy() {
  local allocator=$1 image
  stop_proxy
  if [[ "${allocator}" == jemalloc ]]; then
    image=${JEMALLOC_IMAGE}
  else
    image=${TCMALLOC_IMAGE}
  fi
  PROXY_NAME=${NAME}-${allocator}-r${round}
  docker run --detach --name "${PROXY_NAME}" --network host --read-only \
    --cpus "${CPU_LIMIT}" --memory "${MEMORY_LIMIT}" --memory-swap "${MEMORY_LIMIT}" \
    --ulimit nofile=32768:32768 \
    --cap-drop ALL --cap-add NET_BIND_SERVICE --security-opt no-new-privileges \
    --tmpfs /tmp/pingora:rw,noexec,nosuid,nodev,uid=10001,gid=10001,mode=0700 \
    --volume "${OUTPUT}:/work:ro" --entrypoint /usr/local/bin/pingora \
    "${image}" --config /work/pingora.yaml >/dev/null

  local limits
  limits=$(docker inspect --format '{{.HostConfig.NanoCpus}} {{.HostConfig.Memory}} {{.HostConfig.MemorySwap}}' "${PROXY_NAME}")
  if [[ "${limits}" != "${CPU_NANO} ${MEMORY_BYTES} ${MEMORY_BYTES}" ]]; then
    echo "resource limit mismatch for ${allocator}: got=${limits} expected=${CPU_NANO} ${MEMORY_BYTES} ${MEMORY_BYTES}" >&2
    return 1
  fi

  for _ in {1..100}; do
    if curl --noproxy '*' -fsS -H 'host: bench.test' \
      "http://127.0.0.1:${HTTP_PORT}/bytes/64" -o /dev/null 2>/dev/null; then
      docker inspect "${PROXY_NAME}" >"${OUTPUT}/raw/${allocator}-r${round}-runtime-inspect.json"
      return 0
    fi
    if ! docker inspect --format '{{.State.Running}}' "${PROXY_NAME}" 2>/dev/null | grep -q true; then
      docker logs "${PROXY_NAME}" >&2
      return 1
    fi
    sleep 0.05
  done
  docker logs "${PROXY_NAME}" >&2
  return 1
}

sample_resources() {
  local pid=$1 output=$2 cg
  cg=/sys/fs/cgroup$(awk -F: '$1 == "0" {print $3}' "/proc/${pid}/cgroup")
  while kill -0 "${pid}" 2>/dev/null; do
    local timestamp usage rss=0 member value
    timestamp=$(date +%s%N)
    timestamp=${timestamp::-3}
    usage=$(awk '$1 == "usage_usec" {print $2}' "${cg}/cpu.stat" 2>/dev/null || echo 0)
    while read -r member; do
      [[ -r "/proc/${member}/status" ]] || continue
      value=$(awk '$1 == "VmRSS:" {print $2}' "/proc/${member}/status" 2>/dev/null || true)
      rss=$((rss + ${value:-0}))
    done <"${cg}/cgroup.procs"
    printf '%s %s %s\n' "${timestamp}" "${usage}" "${rss}" >>"${output}"
    sleep 0.1
  done
}

field() {
  local line=$1 key=$2
  sed -nE "s/.*${key}=([^ ]+).*/\\1/p" <<<"${line}"
}

run_case() {
  local allocator=$1 protocol=$2 size=$3 concurrency=$4 round=$5
  local case_id=${allocator}-r${round}-${protocol}-b${size}-c${concurrency}
  local raw=${OUTPUT}/raw/${case_id}.txt
  local warmup_raw=${OUTPUT}/raw/${case_id}.warmup.txt
  local request_log=${OUTPUT}/raw/${case_id}.requests.tsv
  local resources=${OUTPUT}/raw/${case_id}.resources
  local path=/bytes/${size} expected actual url expected_meta actual_meta expected_rc actual_rc
  local expected_prefix=${OUTPUT}/raw/backend-b${size}
  local actual_prefix=${OUTPUT}/raw/${case_id}.curl

  expected_meta=$(curl --noproxy '*' -sS -H 'accept-encoding: identity' \
    -D "${expected_prefix}.headers" -o "${expected_prefix}.body" \
    -w '%{http_code} %{http_version}' "http://127.0.0.1:${BACKEND_PORT}${path}")
  expected_rc=$?
  if [[ "${protocol}" == h2-* ]]; then
    url="https://127.0.0.1:${HTTPS_PORT}${path}"
    actual_meta=$(curl --noproxy '*' -ksS --http2 \
      --resolve "bench.test:${HTTPS_PORT}:127.0.0.1" \
      -H 'accept-encoding: identity' -D "${actual_prefix}.headers" \
      -o "${actual_prefix}.body" -w '%{http_code} %{http_version}' \
      "https://bench.test:${HTTPS_PORT}${path}")
  elif [[ "${protocol}" == h1-*-tls || "${protocol}" == h1-tls-* ]]; then
    url="https://127.0.0.1:${HTTPS_PORT}${path}"
    actual_meta=$(curl --noproxy '*' -ksS --http1.1 \
      --resolve "bench.test:${HTTPS_PORT}:127.0.0.1" \
      -H 'accept-encoding: identity' -D "${actual_prefix}.headers" \
      -o "${actual_prefix}.body" -w '%{http_code} %{http_version}' \
      "https://bench.test:${HTTPS_PORT}${path}")
  else
    url="http://127.0.0.1:${HTTP_PORT}${path}"
    actual_meta=$(curl --noproxy '*' -sS -H 'host: bench.test' \
      -H 'accept-encoding: identity' -D "${actual_prefix}.headers" \
      -o "${actual_prefix}.body" -w '%{http_code} %{http_version}' "${url}")
  fi
  actual_rc=$?
  printf '%s\n' "${expected_meta}" >"${expected_prefix}.meta"
  printf '%s\n' "${actual_meta}" >"${actual_prefix}.meta"
  expected=$(sha256sum "${expected_prefix}.body" | cut -d' ' -f1)
  actual=$(sha256sum "${actual_prefix}.body" | cut -d' ' -f1)
  if ((expected_rc != 0 || actual_rc != 0)) \
    || [[ "${expected_meta%% *}" != 200 || "${actual_meta%% *}" != 200 ]] \
    || { [[ "${protocol}" == h2-* ]] && [[ "${actual_meta#* }" != 2 ]]; } \
    || [[ "${actual}" != "${expected}" ]]; then
    printf '%s\t%s\t%s\t%s\t%s\tFAIL_PREFLIGHT\t0\t0\t0\t0\t0\t0\t0\t0\t0\t0\t0\t1\tNA\t%s\n' \
      "${allocator}" "${protocol}" "${size}" "${concurrency}" "${round}" "${raw}" \
      >>"${OUTPUT}/results.tsv"
    echo "preflight expected_meta=${expected_meta} actual_meta=${actual_meta} expected_sha=${expected} actual_sha=${actual}" >"${raw}"
    FAILURES=$((FAILURES + 1))
    return
  fi

  if [[ "${protocol}" == h1-keepalive || "${protocol}" == h1-tls-keepalive ]]; then
    wrk -t1 -c "${concurrency}" -d "${WARMUP}" -s "${ROOT}/bench/wrk-keepalive.lua" \
      -H 'Host: bench.test' -H 'Accept-Encoding: identity' \
      "${url}" >"${warmup_raw}" 2>&1 || true
  elif [[ "${protocol}" == h1-new-tls ]]; then
    wrk -t1 -c "${concurrency}" -d "${WARMUP}" -s "${ROOT}/bench/wrk-close.lua" \
      -H 'Host: bench.test' -H 'Accept-Encoding: identity' \
      "${url}" >"${warmup_raw}" 2>&1 || true
  fi

  local container_pid sampler_pid rc clients streams
  container_pid=$(docker inspect --format '{{.State.Pid}}' "${PROXY_NAME}")
  : >"${resources}"
  sample_resources "${container_pid}" "${resources}" &
  sampler_pid=$!
  sleep 0.05
  case "${protocol}" in
    h1-keepalive|h1-tls-keepalive)
      wrk --latency -t1 -c "${concurrency}" -d "${DURATION}" \
        -s "${ROOT}/bench/wrk-keepalive.lua" -H 'Host: bench.test' \
        -H 'Accept-Encoding: identity' "${url}" >"${raw}" 2>&1
      rc=$?
      ;;
    h1-new-tls)
      wrk --latency -t1 -c "${concurrency}" -d "${DURATION}" \
        -s "${ROOT}/bench/wrk-close.lua" -H 'Host: bench.test' \
        -H 'Accept-Encoding: identity' "${url}" >"${raw}" 2>&1
      rc=$?
      ;;
    h2-single)
      h2load -D "${DURATION}" --warm-up-time="${WARMUP}" -c 1 -m "${concurrency}" \
        --sni bench.test -H 'host: bench.test' -H 'accept-encoding: identity' \
        --log-file "${request_log}" "${url}" >"${raw}" 2>&1
      rc=$?
      ;;
    h2-multi)
      clients=$((concurrency < 4 ? concurrency : 4))
      streams=$(((concurrency + clients - 1) / clients))
      h2load -D "${DURATION}" --warm-up-time="${WARMUP}" -c "${clients}" -m "${streams}" \
        --sni bench.test -H 'host: bench.test' -H 'accept-encoding: identity' \
        --log-file "${request_log}" "${url}" >"${raw}" 2>&1
      rc=$?
      ;;
  esac
  sleep 0.05
  kill "${sampler_pid}" >/dev/null 2>&1 || true
  wait "${sampler_pid}" >/dev/null 2>&1 || true

  local latency_line resource_line rps errors status distribution incomplete http_errors transport_errors
  if [[ "${protocol}" == h2-* ]]; then
    latency_line=$(python3 "${ROOT}/bench/summarize_h2load.py" "${request_log}" 2>/dev/null || true)
    rps=$(sed -nE 's/.*finished in [^,]+, ([0-9.]+) req\/s.*/\1/p' "${raw}" | tail -1)
    transport_errors=$(sed -nE 's/requests: [0-9]+ total, [0-9]+ started, [0-9]+ done, [0-9]+ succeeded, ([0-9]+) failed.*/\1/p' "${raw}" | tail -1)
    http_errors=$(awk -F '\t' '$2 >= 100 && $2 <= 599 && $2 != 200 {count++} END {print count + 0}' \
      "${request_log}" 2>/dev/null)
    errors=$((${transport_errors:-0} + ${http_errors:-0}))
    distribution=$(field "${latency_line}" statuses)
    incomplete=$(field "${latency_line}" incomplete)
    distribution="${distribution},timing_cutoff:${incomplete:-0}"
  else
    latency_line=$(grep 'LATENCY_US ' "${raw}" | tail -1)
    rps=$(awk '/Requests\/sec:/ {print $2}' "${raw}" | tail -1)
    transport_errors=$(awk '/Socket errors:/ {gsub(/[^0-9 ]/, ""); print $1+$2+$3+$4}' "${raw}" | tail -1)
    http_errors=$(awk '/Non-2xx or 3xx responses:/ {print $5}' "${raw}" | tail -1)
    errors=$((${transport_errors:-0} + ${http_errors:-0}))
    distribution=non_2xx_3xx:${http_errors:-0}
  fi
  resource_line=$(python3 "${ROOT}/bench/summarize_resources.py" "${resources}")
  rps=${rps:-0}
  errors=${errors:-0}
  status=PASS
  if ((rc != 0 || errors != 0)) || [[ "${rps}" == 0 || "${rps}" == 0.00 ]]; then
    status=FAIL
    FAILURES=$((FAILURES + 1))
  fi
  printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
    "${allocator}" "${protocol}" "${size}" "${concurrency}" "${round}" "${status}" "${rps}" \
    "$(field "${latency_line}" p50)" "$(field "${latency_line}" p90)" \
    "$(field "${latency_line}" p95)" "$(field "${latency_line}" p99)" \
    "$(field "${latency_line}" p999)" "$(field "${latency_line}" max)" \
    "$(field "${resource_line}" cpu_avg)" "$(field "${resource_line}" cpu_peak)" \
    "$(field "${resource_line}" rss_avg_kib)" "$(field "${resource_line}" rss_peak_kib)" \
    "${errors}" "${distribution:-NA}" "${raw}" >>"${OUTPUT}/results.tsv"
}

for ((round = 1; round <= ROUNDS; round++)); do
  if ((round % 2 == 1)); then
    ORDER=(jemalloc tcmalloc)
  else
    ORDER=(tcmalloc jemalloc)
  fi
  for allocator in "${ORDER[@]}"; do
    echo "round=${round}/${ROUNDS} allocator=${allocator} limits=${CPU_LIMIT}cpu/${MEMORY_LIMIT}"
    if ! start_proxy "${allocator}"; then
      echo "${allocator} failed to start in round ${round}" >&2
      FAILURES=$((FAILURES + 1))
      stop_proxy
      continue
    fi
    sleep 0.5
    for protocol in "${PROTOCOLS[@]}"; do
      for size in "${PAYLOADS[@]}"; do
        for concurrency in "${CONCURRENCIES[@]}"; do
          run_case "${allocator}" "${protocol}" "${size}" "${concurrency}" "${round}"
        done
      done
    done
    stop_proxy
    sleep 0.5
  done
done

python3 "${ROOT}/bench/summarize_allocator_images.py" \
  "${OUTPUT}/results.tsv" "${OUTPUT}/summary.tsv" >"${OUTPUT}/summary.txt"
cat "${OUTPUT}/summary.txt"
echo "results=${OUTPUT} failures=${FAILURES}"
exit $((FAILURES > 0))
