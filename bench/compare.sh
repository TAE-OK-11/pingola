#!/usr/bin/env bash
set -uo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
PROFILE=${BENCH_PROFILE:-smoke}
PINGORA_IMAGE=${PINGORA_IMAGE:-ghcr.io/tae-ok-11/pingora:local}
NGINX_IMAGE=${NGINX_IMAGE:-tae00217/jbs-nginx:ultra-4.0}
CPU_LIMIT=${BENCH_CPUS:-0.5}
MEMORY_LIMIT=${BENCH_MEMORY:-1g}
MIN_FREE_BYTES=${BENCH_MIN_FREE_BYTES:-1073741824}
BACKEND_PORT=${BACKEND_PORT:-18700}
HTTP_PORT=${HTTP_PORT:-18780}
HTTPS_PORT=${HTTPS_PORT:-18743}
DURATION=${BENCH_DURATION:-3s}
WARMUP=${BENCH_WARMUP:-1s}
STABILITY_REQUESTS=${BENCH_STABILITY_REQUESTS:-480}
OUTPUT=${BENCH_OUTPUT:-${ROOT}/bench/results/$(date -u +%Y%m%dT%H%M%SZ)}
if [[ "${OUTPUT}" != /* ]]; then
  OUTPUT=${ROOT}/${OUTPUT}
fi
NAME=pingora-compare-$$
BACKEND_PID=
PROXY_NAME=
FAILURES=0
declare -A PREFLIGHT_RESULT
declare -A PREFLIGHT_DETAIL

case "${PROFILE}" in
  smoke)
    ROUNDS=${BENCH_ROUNDS:-1}
    PROTOCOLS=(h1-keepalive h2-single h2-multi)
    PAYLOADS=(64 4096)
    CONCURRENCIES=(1 8 32)
    ;;
  full)
    ROUNDS=${BENCH_ROUNDS:-5}
    PROTOCOLS=(h1-keepalive h1-new h1-new-tls h2-single h2-multi)
    PAYLOADS=(0 64 1024 4096 65536 1048576 10485760 104857600)
    CONCURRENCIES=(1 4 8 16 32 64 128)
    ;;
  *)
    echo "BENCH_PROFILE must be smoke or full" >&2
    exit 2
    ;;
esac

cleanup() {
  if [[ -n "${PROXY_NAME}" ]]; then
    docker logs "${PROXY_NAME}" >"${OUTPUT}/${PROXY_NAME}.final.log" 2>&1 || true
    docker rm -f "${PROXY_NAME}" >/dev/null 2>&1 || true
  fi
  if [[ -n "${BACKEND_PID}" ]]; then
    kill "${BACKEND_PID}" >/dev/null 2>&1 || true
    wait "${BACKEND_PID}" >/dev/null 2>&1 || true
  fi
}
handle_signal() {
  trap - EXIT INT TERM
  cleanup
  exit 130
}
trap cleanup EXIT
trap handle_signal INT TERM

mkdir -p "${OUTPUT}/raw"
chmod 0755 "${OUTPUT}" "${OUTPUT}/raw"
AVAILABLE_BYTES=$(df --output=avail -B1 "${OUTPUT}" | tail -1 | tr -d ' ')
if ((AVAILABLE_BYTES < MIN_FREE_BYTES)); then
  echo "insufficient benchmark disk space: available=${AVAILABLE_BYTES} required=${MIN_FREE_BYTES} path=${OUTPUT}" >&2
  exit 2
fi
chmod +x "${ROOT}/bench/backend.py" "${ROOT}/bench/summarize_h2load.py" \
  "${ROOT}/bench/summarize_resources.py" "${ROOT}/bench/summarize_compare.py"

for command in docker curl wrk h2load openssl python3 sha256sum jq numfmt; do
  if ! command -v "${command}" >/dev/null; then
    echo "missing required command: ${command}" >&2
    exit 2
  fi
done

CPU_NANO=$(python3 - "${CPU_LIMIT}" <<'PY'
import sys
print(int(float(sys.argv[1]) * 1_000_000_000))
PY
)
MEMORY_BYTES=$(numfmt --from=iec "${MEMORY_LIMIT^^}")

cat >"${OUTPUT}/environment.txt" <<EOF
timestamp=$(date -u +%FT%TZ)
profile=${PROFILE}
host=$(uname -a)
pingora_image=${PINGORA_IMAGE}
nginx_image=${NGINX_IMAGE}
cpu_limit=${CPU_LIMIT}
cpu_nano=${CPU_NANO}
memory_limit=${MEMORY_LIMIT}
memory_bytes=${MEMORY_BYTES}
minimum_free_bytes=${MIN_FREE_BYTES}
available_bytes_at_start=${AVAILABLE_BYTES}
stability_requests=${STABILITY_REQUESTS}
note=load generator and backend share this host; each proxy container has identical CPU and memory limits
EOF
lscpu >>"${OUTPUT}/environment.txt" 2>&1
docker image inspect "${PINGORA_IMAGE}" >"${OUTPUT}/pingora-inspect.json"
docker image inspect "${NGINX_IMAGE}" >"${OUTPUT}/nginx-inspect.json"
docker history --no-trunc "${NGINX_IMAGE}" >"${OUTPUT}/nginx-history.txt"
docker run --rm "${NGINX_IMAGE}" nginx -V >"${OUTPUT}/nginx-V.txt" 2>&1
docker run --rm --entrypoint sh "${NGINX_IMAGE}" -c \
  'ldd /usr/sbin/nginx 2>/dev/null || ldd /usr/local/nginx/sbin/nginx; nginx -T' \
  >"${OUTPUT}/nginx-runtime.txt" 2>&1 || true
docker run --rm --entrypoint /usr/local/bin/pingora "${PINGORA_IMAGE}" --allocator-info \
  >"${OUTPUT}/pingora-allocator.txt"

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
  downstream_keepalive_requests: 1000000
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
    read_timeout_seconds: 3600
    write_timeout_seconds: 3600
    idle_timeout_seconds: 30
hosts:
  bench:
    domains: ["bench.test"]
    handler: vaultwarden
    upstream: backend
    max_body_bytes: 536870912
route_limits:
  vaultwarden:
    rate_per_second: 0
    active_requests: 0
EOF

cat >"${OUTPUT}/nginx.conf" <<EOF
user nginx;
worker_processes 1;
error_log /dev/stderr warn;
pid /tmp/nginx.pid;
events { worker_connections 8192; }
http {
  access_log off;
  server_tokens off;
  sendfile on;
  tcp_nopush on;
  tcp_nodelay on;
  keepalive_timeout 30s;
  keepalive_requests 1000000;
  gzip off;
  brotli off;
  zstd off;
  map \$https \$forwarded_ssl {
    default off;
    on on;
  }
  map \$https \$hsts {
    default "";
    on "max-age=63072000; includeSubDomains; preload";
  }
  proxy_http_version 1.1;
  proxy_buffering off;
  proxy_request_buffering off;
  proxy_connect_timeout 2s;
  proxy_read_timeout 3600s;
  proxy_send_timeout 3600s;
  proxy_set_header Host \$host;
  proxy_set_header X-Real-IP \$remote_addr;
  proxy_set_header X-Forwarded-For \$remote_addr;
  proxy_set_header X-Forwarded-Host \$host;
  proxy_set_header X-Forwarded-Port \$server_port;
  proxy_set_header X-Forwarded-Proto \$scheme;
  proxy_set_header X-Forwarded-Ssl \$forwarded_ssl;
  proxy_set_header Accept-Encoding "";
  proxy_set_header Upgrade "";
  proxy_set_header Connection "";
  proxy_hide_header Server;
  proxy_hide_header X-Powered-By;
  proxy_hide_header Alt-Svc;
  proxy_hide_header Strict-Transport-Security;
  proxy_hide_header X-Content-Type-Options;
  proxy_hide_header X-Frame-Options;
  proxy_hide_header Referrer-Policy;
  upstream benchmark_backend {
    server 127.0.0.1:${BACKEND_PORT};
    keepalive 128;
    keepalive_requests 2000;
    keepalive_timeout 30s;
  }
  server {
    listen 127.0.0.1:${HTTP_PORT};
    listen 127.0.0.1:${HTTPS_PORT} ssl;
    http2 on;
    server_name bench.test;
    ssl_certificate /work/cert.pem;
    ssl_certificate_key /work/key.pem;
    ssl_protocols TLSv1.3;
    add_header X-Content-Type-Options nosniff always;
    add_header X-Frame-Options SAMEORIGIN always;
    add_header Referrer-Policy strict-origin-when-cross-origin always;
    add_header Strict-Transport-Security \$hsts always;
    location / { proxy_pass http://benchmark_backend; }
  }
}
EOF
chmod 0644 "${OUTPUT}/pingora.yaml" "${OUTPUT}/nginx.conf"

python3 "${ROOT}/bench/backend.py" --port "${BACKEND_PORT}" \
  >"${OUTPUT}/backend.stdout" 2>"${OUTPUT}/backend.stderr" &
BACKEND_PID=$!
backend_ready=false
for _ in {1..100}; do
  if curl --noproxy '*' -fsS "http://127.0.0.1:${BACKEND_PORT}/bytes/64" -o /dev/null 2>/dev/null; then
    backend_ready=true
    break
  fi
  sleep 0.05
done
if [[ "${backend_ready}" != true ]]; then
  echo "benchmark backend did not become ready at 127.0.0.1:${BACKEND_PORT}" >&2
  exit 1
fi

printf 'proxy\tprotocol\tpayload_bytes\tconcurrency\tround\tstatus\trps\tp50_us\tp90_us\tp95_us\tp99_us\tp999_us\tmax_us\tcpu_avg_pct\tcpu_peak_pct\trss_avg_kib\trss_peak_kib\terrors\tstatus_distribution\traw\n' \
  >"${OUTPUT}/results.tsv"
printf 'proxy\tround\tprobe\tstatus\terrors\traw\n' >"${OUTPUT}/stability.tsv"

stop_proxy() {
  if [[ -n "${PROXY_NAME}" ]]; then
    docker logs "${PROXY_NAME}" >"${OUTPUT}/raw/${PROXY_NAME}.log" 2>&1 || true
    docker rm -f "${PROXY_NAME}" >/dev/null 2>&1 || true
    PROXY_NAME=
  fi
}

start_proxy() {
  local proxy=$1 round=$2
  stop_proxy
  PROXY_NAME=${NAME}-${proxy}-r${round}
  if [[ "${proxy}" == pingora ]]; then
    docker run --detach --name "${PROXY_NAME}" --network host --read-only \
      --cpus "${CPU_LIMIT}" --memory "${MEMORY_LIMIT}" --memory-swap "${MEMORY_LIMIT}" \
      --ulimit nofile=32768:32768 \
      --cap-drop ALL --cap-add NET_BIND_SERVICE --security-opt no-new-privileges \
      --tmpfs /tmp/pingora:rw,noexec,nosuid,nodev,uid=10001,gid=10001,mode=0700 \
      --volume "${OUTPUT}:/work:ro" --entrypoint /usr/local/bin/pingora \
      "${PINGORA_IMAGE}" --config /work/pingora.yaml >/dev/null
  else
    docker run --detach --name "${PROXY_NAME}" --network host \
      --cpus "${CPU_LIMIT}" --memory "${MEMORY_LIMIT}" --memory-swap "${MEMORY_LIMIT}" \
      --ulimit nofile=32768:32768 \
      --volume "${OUTPUT}:/work:ro" --volume "${OUTPUT}/nginx.conf:/etc/nginx/nginx.conf:ro" \
      "${NGINX_IMAGE}" >/dev/null
  fi

  local limits
  limits=$(docker inspect --format '{{.HostConfig.NanoCpus}} {{.HostConfig.Memory}} {{.HostConfig.MemorySwap}}' "${PROXY_NAME}")
  if [[ "${limits}" != "${CPU_NANO} ${MEMORY_BYTES} ${MEMORY_BYTES}" ]]; then
    echo "resource limit mismatch for ${proxy}: got=${limits} expected=${CPU_NANO} ${MEMORY_BYTES} ${MEMORY_BYTES}" >&2
    return 1
  fi

  for _ in {1..100}; do
    if curl --noproxy '*' -fsS -H 'host: bench.test' \
      "http://127.0.0.1:${HTTP_PORT}/bytes/64" -o /dev/null 2>/dev/null; then
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
  local pid=$1
  local output=$2
  local cg
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
  local proxy=$1 protocol=$2 size=$3 concurrency=$4 round=$5
  local case_id=${proxy}-r${round}-${protocol}-b${size}-c${concurrency}
  local raw=${OUTPUT}/raw/${case_id}.txt
  local warmup_raw=${OUTPUT}/raw/${case_id}.warmup.txt
  local request_log=${OUTPUT}/raw/${case_id}.requests.tsv
  local resources=${OUTPUT}/raw/${case_id}.resources
  local path expected actual curl_args=() url expected_status
  if ((size == 0)); then
    path=/status/204
    expected_status=204
  elif ((size >= 10485760)); then
    path=/stream/${size}
    expected_status=200
  else
    path=/bytes/${size}
    expected_status=200
  fi
  if [[ "${protocol}" == h2-* || "${protocol}" == h1-new-tls ]]; then
    url="https://127.0.0.1:${HTTPS_PORT}${path}"
    curl_args=(-k --resolve "bench.test:${HTTPS_PORT}:127.0.0.1")
  else
    url="http://127.0.0.1:${HTTP_PORT}${path}"
  fi
  local preflight_key=${proxy}-r${round}-${protocol}-b${size}
  if [[ -z "${PREFLIGHT_RESULT[${preflight_key}]+present}" ]]; then
    local expected_prefix=${OUTPUT}/raw/backend-b${size}
    local actual_prefix=${OUTPUT}/raw/${preflight_key}.curl
    local expected_meta actual_meta expected_rc actual_rc actual_version
    if [[ ! -s "${expected_prefix}.sha256" ]]; then
      expected_meta=$(curl --noproxy '*' -sS -H 'accept-encoding: identity' \
        -D "${expected_prefix}.headers" -o "${expected_prefix}.body" \
        -w '%{http_code} %{http_version}' \
        "http://127.0.0.1:${BACKEND_PORT}${path}")
      expected_rc=$?
      printf '%s\n' "${expected_meta}" >"${expected_prefix}.meta"
      if ((expected_rc != 0)) || [[ "${expected_meta%% *}" != "${expected_status}" ]]; then
        PREFLIGHT_RESULT[${preflight_key}]=FAIL
        PREFLIGHT_DETAIL[${preflight_key}]="backend curl failed rc=${expected_rc} meta=${expected_meta} expected_status=${expected_status}"
      else
        sha256sum "${expected_prefix}.body" | cut -d' ' -f1 >"${expected_prefix}.sha256"
      fi
    fi

    if [[ "${PREFLIGHT_RESULT[${preflight_key}]:-PASS}" == PASS ]]; then
      if [[ "${protocol}" == h2-* ]]; then
        actual_meta=$(curl --noproxy '*' -ksS --http2 \
          --resolve "bench.test:${HTTPS_PORT}:127.0.0.1" \
          -H 'accept-encoding: identity' -D "${actual_prefix}.headers" \
          -o "${actual_prefix}.body" -w '%{http_code} %{http_version}' \
          "https://bench.test:${HTTPS_PORT}${path}")
      else
        actual_meta=$(curl --noproxy '*' -sS "${curl_args[@]}" \
          -H 'host: bench.test' -H 'accept-encoding: identity' \
          -D "${actual_prefix}.headers" -o "${actual_prefix}.body" \
          -w '%{http_code} %{http_version}' "${url}")
      fi
      actual_rc=$?
      printf '%s\n' "${actual_meta}" >"${actual_prefix}.meta"
      actual_version=${actual_meta#* }
      expected=$(<"${expected_prefix}.sha256")
      actual=$(sha256sum "${actual_prefix}.body" | cut -d' ' -f1)
      printf '%s\n' "${actual}" >"${actual_prefix}.sha256"
      if ((actual_rc != 0)) \
        || [[ "${actual_meta%% *}" != "${expected_status}" ]] \
        || { [[ "${protocol}" == h2-* ]] && [[ "${actual_version}" != 2 ]]; } \
        || [[ "${actual}" != "${expected}" ]]; then
        PREFLIGHT_RESULT[${preflight_key}]=FAIL
        PREFLIGHT_DETAIL[${preflight_key}]="proxy curl failed rc=${actual_rc} meta=${actual_meta} expected_status=${expected_status} expected_sha=${expected} actual_sha=${actual}"
      else
        PREFLIGHT_RESULT[${preflight_key}]=PASS
        PREFLIGHT_DETAIL[${preflight_key}]="status=${expected_status} http_version=${actual_version} sha256=${actual}"
      fi
      if ((size > 1048576)); then
        rm -f "${expected_prefix}.body" "${actual_prefix}.body"
      fi
    fi
  fi
  if [[ "${PREFLIGHT_RESULT[${preflight_key}]}" != PASS ]]; then
    printf '%s\t%s\t%s\t%s\t%s\tFAIL_PREFLIGHT\t0\tNA\tNA\tNA\tNA\tNA\tNA\t0\t0\t0\t0\t1\tNA\t%s\n' \
      "${proxy}" "${protocol}" "${size}" "${concurrency}" "${round}" "${raw}" \
      >>"${OUTPUT}/results.tsv"
    printf '%s\n' "${PREFLIGHT_DETAIL[${preflight_key}]}" >"${raw}"
    FAILURES=$((FAILURES + 1))
    return
  fi

  case "${protocol}" in
    h1-keepalive)
      wrk -t1 -c "${concurrency}" -d "${WARMUP}" \
        -s "${ROOT}/bench/wrk-keepalive.lua" -H 'Host: bench.test' \
        -H 'Accept-Encoding: identity' "${url}" \
        >"${warmup_raw}" 2>&1 || true
      ;;
    h1-new|h1-new-tls)
      wrk -t1 -c "${concurrency}" -d "${WARMUP}" \
        -s "${ROOT}/bench/wrk-close.lua" -H 'Host: bench.test' \
        -H 'Accept-Encoding: identity' "${url}" \
        >"${warmup_raw}" 2>&1 || true
      ;;
    h2-single)
      h2load -n "${concurrency}" -c 1 -m "${concurrency}" --sni bench.test \
        -H 'host: bench.test' -H 'accept-encoding: identity' \
        "${url}" >"${warmup_raw}" 2>&1 || true
      ;;
    h2-multi)
      local warm_clients=$((concurrency < 4 ? concurrency : 4))
      local warm_streams=$(((concurrency + warm_clients - 1) / warm_clients))
      h2load -n "${concurrency}" -c "${warm_clients}" -m "${warm_streams}" \
        --sni bench.test -H 'host: bench.test' -H 'accept-encoding: identity' \
        "${url}" >"${warmup_raw}" 2>&1 || true
      ;;
  esac

  local container_pid sampler_pid rc
  container_pid=$(docker inspect --format '{{.State.Pid}}' "${PROXY_NAME}")
  : >"${resources}"
  sample_resources "${container_pid}" "${resources}" &
  sampler_pid=$!
  sleep 0.05
  case "${protocol}" in
    h1-keepalive)
      wrk --latency -t1 -c "${concurrency}" -d "${DURATION}" \
        -s "${ROOT}/bench/wrk-keepalive.lua" -H 'Host: bench.test' \
        -H 'Accept-Encoding: identity' "${url}" >"${raw}" 2>&1
      rc=$?
      ;;
    h1-new|h1-new-tls)
      wrk --latency -t1 -c "${concurrency}" -d "${DURATION}" \
        -s "${ROOT}/bench/wrk-close.lua" -H 'Host: bench.test' \
        -H 'Accept-Encoding: identity' "${url}" >"${raw}" 2>&1
      rc=$?
      ;;
    h2-single)
      if ((size < 10485760)); then
        h2load -D "${DURATION}" --warm-up-time="${WARMUP}" -c 1 -m "${concurrency}" \
          --sni bench.test -H 'host: bench.test' -H 'accept-encoding: identity' \
          --log-file "${request_log}" "${url}" >"${raw}" 2>&1
      else
        local requests=$((concurrency * 2))
        ((size >= 104857600)) && requests=${concurrency}
        h2load -n "${requests}" -c 1 -m "${concurrency}" \
          --sni bench.test -H 'host: bench.test' -H 'accept-encoding: identity' \
          --log-file "${request_log}" "${url}" >"${raw}" 2>&1
      fi
      rc=$?
      ;;
    h2-multi)
      local clients=$((concurrency < 4 ? concurrency : 4))
      local streams=$(((concurrency + clients - 1) / clients))
      if ((size < 10485760)); then
        h2load -D "${DURATION}" --warm-up-time="${WARMUP}" \
          -c "${clients}" -m "${streams}" --sni bench.test \
          -H 'host: bench.test' -H 'accept-encoding: identity' \
          --log-file "${request_log}" "${url}" >"${raw}" 2>&1
      else
        local requests=$((concurrency * 2))
        ((size >= 104857600)) && requests=${concurrency}
        h2load -n "${requests}" -c "${clients}" -m "${streams}" \
          --sni bench.test -H 'host: bench.test' -H 'accept-encoding: identity' \
          --log-file "${request_log}" "${url}" >"${raw}" 2>&1
      fi
      rc=$?
      ;;
  esac
  sleep 0.05
  kill "${sampler_pid}" >/dev/null 2>&1 || true
  wait "${sampler_pid}" >/dev/null 2>&1 || true

  local latency_line resource_line rps errors status distribution http_errors transport_errors
  if [[ "${protocol}" == h2-* ]]; then
    latency_line=$(python3 "${ROOT}/bench/summarize_h2load.py" "${request_log}" 2>/dev/null || true)
    rps=$(sed -nE 's/.*finished in [^,]+, ([0-9.]+) req\/s.*/\1/p' "${raw}" | tail -1)
    transport_errors=$(sed -nE 's/requests: [0-9]+ total, [0-9]+ started, [0-9]+ done, [0-9]+ succeeded, ([0-9]+) failed.*/\1/p' "${raw}" | tail -1)
    http_errors=$(awk -F '\t' -v expected="${expected_status}" \
      '$2 >= 100 && $2 <= 599 && $2 != expected {count++} END {print count + 0}' \
      "${request_log}" 2>/dev/null)
    errors=$((${transport_errors:-0} + ${http_errors:-0}))
    distribution=$(field "${latency_line}" statuses)
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
    "${proxy}" "${protocol}" "${size}" "${concurrency}" "${round}" "${status}" "${rps}" \
    "$(field "${latency_line}" p50)" "$(field "${latency_line}" p90)" \
    "$(field "${latency_line}" p95)" "$(field "${latency_line}" p99)" \
    "$(field "${latency_line}" p999)" "$(field "${latency_line}" max)" \
    "$(field "${resource_line}" cpu_avg)" "$(field "${resource_line}" cpu_peak)" \
    "$(field "${resource_line}" rss_avg_kib)" "$(field "${resource_line}" rss_peak_kib)" \
    "${errors}" "${distribution:-NA}" "${raw}" >>"${OUTPUT}/results.tsv"
}

for ((round = 1; round <= ROUNDS; round++)); do
  if ((round % 2 == 1)); then
    ORDER=(nginx pingora)
  else
    ORDER=(pingora nginx)
  fi
  for proxy in "${ORDER[@]}"; do
    if ! start_proxy "${proxy}" "${round}"; then
      echo "${proxy} failed to start in round ${round}" >&2
      FAILURES=$((FAILURES + 1))
      stop_proxy
      continue
    fi
    curl --noproxy '*' -fsS -H 'host: bench.test' \
      "http://127.0.0.1:${HTTP_PORT}/bytes/64" -o /dev/null || true
    sleep 0.5
    stability_raw=${OUTPUT}/raw/${proxy}-r${round}-h2-single-connection-${STABILITY_REQUESTS}.txt
    stability_log=${OUTPUT}/raw/${proxy}-r${round}-h2-single-connection-${STABILITY_REQUESTS}.requests.tsv
    h2load -n "${STABILITY_REQUESTS}" -c 1 -m 32 --sni bench.test \
      -H 'host: bench.test' -H 'accept-encoding: identity' --log-file "${stability_log}" \
      "https://127.0.0.1:${HTTPS_PORT}/bytes/64" >"${stability_raw}" 2>&1
    stability_rc=$?
    stability_errors=$(sed -nE 's/requests: [0-9]+ total, [0-9]+ started, [0-9]+ done, [0-9]+ succeeded, ([0-9]+) failed.*/\1/p' \
      "${stability_raw}" | tail -1)
    stability_errors=${stability_errors:-${STABILITY_REQUESTS}}
    stability_http_errors=$(awk -F '\t' \
      '$2 >= 100 && $2 <= 599 && $2 != 200 {count++} END {print count + 0}' \
      "${stability_log}" 2>/dev/null)
    stability_errors=$((stability_errors + ${stability_http_errors:-0}))
    stability_status=PASS
    if ((stability_rc != 0 || stability_errors != 0)); then
      stability_status=FAIL
      FAILURES=$((FAILURES + 1))
    fi
    printf '%s\t%s\th2-single-connection-%s\t%s\t%s\t%s\n' \
      "${proxy}" "${round}" "${STABILITY_REQUESTS}" "${stability_status}" "${stability_errors}" "${stability_raw}" \
      >>"${OUTPUT}/stability.tsv"
    for protocol in "${PROTOCOLS[@]}"; do
      for size in "${PAYLOADS[@]}"; do
        for concurrency in "${CONCURRENCIES[@]}"; do
          run_case "${proxy}" "${protocol}" "${size}" "${concurrency}" "${round}"
        done
      done
    done
    stop_proxy
    sleep 0.5
  done
done

python3 "${ROOT}/bench/summarize_compare.py" \
  "${OUTPUT}/results.tsv" "${OUTPUT}/summary.tsv" >"${OUTPUT}/summary.txt"
cat "${OUTPUT}/summary.txt"
echo "results=${OUTPUT}/results.tsv failures=${FAILURES}"
exit $((FAILURES > 0))
