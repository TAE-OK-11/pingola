#!/usr/bin/env bash
set -uo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
ARTIFACTS=${1:?usage: compare.sh ARTIFACT_DIR [OUTPUT_DIR]}
OUTPUT=${2:-${ROOT}/bench/results/candidate-compare-$(date -u +%Y%m%dT%H%M%SZ)}
BACKEND_PORT=${BACKEND_PORT:-18700}
HTTP_PORT=${HTTP_PORT:-80}
HTTPS_PORT=${HTTPS_PORT:-443}
WORKERS=${BENCH_WORKERS:-$(nproc)}
DURATION=${BENCH_DURATION:-3s}
WARMUP=${BENCH_WARMUP:-1s}
ROUNDS=${BENCH_ROUNDS:-3}
BACKEND_PID=
PROXY_PID=
FAILURES=0
read -r -a CANDIDATES <<<"${BENCH_CANDIDATES:-pingora pingap aralez pingpong zentinel}"
read -r -a PROTOCOLS <<<"${BENCH_PROTOCOLS:-h1-keepalive h2-single h2-multi}"
read -r -a PAYLOADS <<<"${BENCH_PAYLOADS:-64 4096}"
read -r -a CONCURRENCIES <<<"${BENCH_CONCURRENCIES:-1 8 32}"

cleanup_proxy() {
  if [[ -n "${PROXY_PID}" ]]; then
    kill -TERM "${PROXY_PID}" >/dev/null 2>&1 || true
    for _ in {1..40}; do
      kill -0 "${PROXY_PID}" >/dev/null 2>&1 || break
      sleep 0.05
    done
    kill -KILL "${PROXY_PID}" >/dev/null 2>&1 || true
    wait "${PROXY_PID}" >/dev/null 2>&1 || true
    PROXY_PID=
  fi
}

cleanup() {
  cleanup_proxy
  if [[ -n "${BACKEND_PID}" ]]; then
    kill -TERM "${BACKEND_PID}" >/dev/null 2>&1 || true
    wait "${BACKEND_PID}" >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT
trap 'exit 130' INT TERM

for command in curl getconf h2load nproc openssl python3 sha256sum wrk; do
  command -v "${command}" >/dev/null || {
    echo "missing required command: ${command}" >&2
    exit 2
  }
done
for binary in \
  "${ARTIFACTS}/tools/backend" \
  "${ARTIFACTS}/jbs/pingora" \
  "${ARTIFACTS}/pingap/pingap" \
  "${ARTIFACTS}/aralez/aralez" \
  "${ARTIFACTS}/pingpong/pingpong" \
  "${ARTIFACTS}/zentinel/zentinel"; do
  [[ -x "${binary}" ]] || {
    echo "missing Actions-built binary: ${binary}" >&2
    exit 2
  }
done
[[ "${ROUNDS}" =~ ^[1-9][0-9]*$ ]] || {
  echo "BENCH_ROUNDS must be a positive integer" >&2
  exit 2
}
for candidate in "${CANDIDATES[@]}"; do
  case "${candidate}" in
    pingora|pingap|aralez|pingpong|zentinel) ;;
    *)
      echo "unsupported BENCH_CANDIDATES entry: ${candidate}" >&2
      exit 2
      ;;
  esac
done

install -d "${OUTPUT}/raw" "${OUTPUT}/provenance"
openssl req -x509 -newkey rsa:2048 -nodes -days 1 \
  -subj '/CN=bench.test' -addext 'subjectAltName=DNS:bench.test' \
  -keyout "${OUTPUT}/key.pem" -out "${OUTPUT}/cert.pem" >/dev/null 2>&1
chmod 0600 "${OUTPUT}/key.pem"

{
  echo "timestamp=$(date -u +%FT%TZ)"
  echo "host=$(uname -a)"
  echo "workers=${WORKERS}"
  echo "rounds=${ROUNDS}"
  echo "duration=${DURATION}"
  echo "warmup=${WARMUP}"
  echo "http_port=${HTTP_PORT}"
  echo "https_port=${HTTPS_PORT}"
  echo "backend_port=${BACKEND_PORT}"
  echo "backend=Actions-built dependency-free Rust HTTP/1.1"
  echo "note=load generator, backend, and candidate share all host CPUs; no cgroup CPU or RAM limit"
  lscpu
  free -h
  swapon --show
} >"${OUTPUT}/environment.txt"
for candidate in pingap aralez pingpong zentinel; do
  cp "${ARTIFACTS}/${candidate}/build-manifest.txt" \
    "${OUTPUT}/provenance/${candidate}-build-manifest.txt"
  sha256sum "${ARTIFACTS}/${candidate}/${candidate}" \
    >"${OUTPUT}/provenance/${candidate}-sha256.txt"
  ldd "${ARTIFACTS}/${candidate}/${candidate}" \
    >"${OUTPUT}/provenance/${candidate}-ldd.txt" 2>&1 || true
done
sha256sum "${ARTIFACTS}/jbs/pingora" >"${OUTPUT}/provenance/pingora-sha256.txt"
ldd "${ARTIFACTS}/jbs/pingora" >"${OUTPUT}/provenance/pingora-ldd.txt" 2>&1 || true

"${ARTIFACTS}/tools/backend" --port "${BACKEND_PORT}" \
  >"${OUTPUT}/backend.stdout" 2>"${OUTPUT}/backend.stderr" &
BACKEND_PID=$!
backend_ready=false
for _ in {1..100}; do
  if curl --noproxy '*' -fsS "http://127.0.0.1:${BACKEND_PORT}/health" \
    -o /dev/null 2>/dev/null; then
    backend_ready=true
    break
  fi
  sleep 0.05
done
[[ "${backend_ready}" == true ]] || {
  echo "benchmark backend failed to start" >&2
  exit 2
}
for size in "${PAYLOADS[@]}"; do
  curl --noproxy '*' -fsS "http://127.0.0.1:${BACKEND_PORT}/bytes/${size}" \
    -o "${OUTPUT}/backend-${size}.body"
  sha256sum "${OUTPUT}/backend-${size}.body" | cut -d' ' -f1 \
    >"${OUTPUT}/backend-${size}.sha256"
done

printf 'proxy\tprotocol\tpayload_bytes\tconcurrency\tround\tstatus\trps\tp50_us\tp90_us\tp95_us\tp99_us\tp999_us\tmax_us\tcpu_avg_pct\tcpu_peak_pct\trss_avg_kib\trss_peak_kib\terrors\tstatus_distribution\traw\n' \
  >"${OUTPUT}/results.tsv"

start_candidate() {
  local candidate=$1 round=$2
  local config_dir=${OUTPUT}/${candidate}
  cleanup_proxy
  "${ROOT}/bench/candidates/configure.sh" \
    "${candidate}" "${config_dir}" "${HTTP_PORT}" "${HTTPS_PORT}" \
    "${BACKEND_PORT}" "${OUTPUT}/cert.pem" "${OUTPUT}/key.pem" "${WORKERS}"
  case "${candidate}" in
    pingora)
      candidate_command=("${ARTIFACTS}/jbs/pingora" --config "${config_dir}/pingora.yaml")
      ;;
    pingap)
      candidate_command=("${ARTIFACTS}/pingap/pingap" -c "${config_dir}/pingap.toml")
      ;;
    aralez)
      candidate_command=("${ARTIFACTS}/aralez/aralez" -c "${config_dir}/main.yaml")
      ;;
    pingpong)
      candidate_command=("${ARTIFACTS}/pingpong/pingpong" -c "${config_dir}/pingpong.toml")
      ;;
    zentinel)
      candidate_command=(env RUST_LOG=error "${ARTIFACTS}/zentinel/zentinel" -c "${config_dir}/zentinel.kdl")
      ;;
  esac
  "${candidate_command[@]}" >"${OUTPUT}/raw/${candidate}-r${round}.stdout" \
    2>"${OUTPUT}/raw/${candidate}-r${round}.stderr" &
  PROXY_PID=$!
  for _ in {1..200}; do
    if curl --noproxy '*' -fsS -H 'host: bench.test' \
      "http://127.0.0.1:${HTTP_PORT}/bytes/64" -o /dev/null 2>/dev/null; then
      return 0
    fi
    kill -0 "${PROXY_PID}" >/dev/null 2>&1 || return 1
    sleep 0.05
  done
  return 1
}

sample_resources() {
  local pid=$1 output=$2
  local clock_ticks timestamp ticks usage rss
  clock_ticks=$(getconf CLK_TCK)
  while [[ -r "/proc/${pid}/stat" ]]; do
    timestamp=$(date +%s%N)
    timestamp=${timestamp::-3}
    ticks=$(awk '{print $14 + $15}' "/proc/${pid}/stat" 2>/dev/null || echo 0)
    usage=$((ticks * 1000000 / clock_ticks))
    rss=$(awk '$1 == "VmRSS:" {print $2}' "/proc/${pid}/status" 2>/dev/null || echo 0)
    printf '%s %s %s\n' "${timestamp}" "${usage}" "${rss:-0}" >>"${output}"
    sleep 0.1
  done
}

field() {
  local line=$1 key=$2
  sed -nE "s/.*${key}=([^ ]+).*/\\1/p" <<<"${line}"
}

record_skip() {
  local candidate=$1 protocol=$2 size=$3 concurrency=$4 round=$5 status=$6 raw=$7
  printf '%s\t%s\t%s\t%s\t%s\t%s\t0\tNA\tNA\tNA\tNA\tNA\tNA\t0\t0\t0\t0\t0\tNA\t%s\n' \
    "${candidate}" "${protocol}" "${size}" "${concurrency}" "${round}" \
    "${status}" "${raw}" >>"${OUTPUT}/results.tsv"
}

run_case() {
  local candidate=$1 protocol=$2 size=$3 concurrency=$4 round=$5
  local case_id=${candidate}-r${round}-${protocol}-b${size}-c${concurrency}
  local raw=${OUTPUT}/raw/${case_id}.txt
  local warmup_raw=${OUTPUT}/raw/${case_id}.warmup.txt
  local request_log=${OUTPUT}/raw/${case_id}.requests.tsv
  local resources=${OUTPUT}/raw/${case_id}.resources
  local url
  if [[ "${candidate}" == pingpong && "${protocol}" == h2-* ]]; then
    echo 'Pingpong does not advertise h2 through ALPN at the pinned revision.' >"${raw}"
    record_skip "${candidate}" "${protocol}" "${size}" "${concurrency}" \
      "${round}" SKIP_UNSUPPORTED "${raw}"
    return
  fi
  if [[ "${protocol}" == h2-* ]]; then
    url="https://bench.test:${HTTPS_PORT}/bytes/${size}"
  else
    url="http://127.0.0.1:${HTTP_PORT}/bytes/${size}"
  fi

  local expected actual meta curl_rc
  expected=$(<"${OUTPUT}/backend-${size}.sha256")
  set +e
  if [[ "${protocol}" == h2-* ]]; then
    meta=$(curl --noproxy '*' -ksS --http2 \
      --resolve "bench.test:${HTTPS_PORT}:127.0.0.1" \
      -H 'accept-encoding: identity' -o "${raw}.body" \
      -w '%{http_code} %{http_version}' "${url}")
  else
    meta=$(curl --noproxy '*' -sS --http1.1 -H 'host: bench.test' \
      -H 'accept-encoding: identity' -o "${raw}.body" \
      -w '%{http_code} %{http_version}' "${url}")
  fi
  curl_rc=$?
  set -e
  actual=$(sha256sum "${raw}.body" 2>/dev/null | cut -d' ' -f1)
  if ((curl_rc != 0)) || [[ "${meta%% *}" != 200 ]] \
    || { [[ "${protocol}" == h2-* ]] && [[ "${meta#* }" != 2 ]]; } \
    || [[ "${actual}" != "${expected}" ]]; then
    printf 'preflight failed: curl_rc=%s meta=%s expected_sha=%s actual_sha=%s\n' \
      "${curl_rc}" "${meta}" "${expected}" "${actual}" >"${raw}"
    record_skip "${candidate}" "${protocol}" "${size}" "${concurrency}" \
      "${round}" FAIL_PREFLIGHT "${raw}"
    FAILURES=$((FAILURES + 1))
    return
  fi
  rm -f "${raw}.body"

  case "${protocol}" in
    h1-keepalive)
      wrk -t1 -c "${concurrency}" -d "${WARMUP}" \
        -s "${ROOT}/bench/wrk-keepalive.lua" -H 'Host: bench.test' \
        -H 'Accept-Encoding: identity' "${url}" >"${warmup_raw}" 2>&1 || true
      ;;
    h2-single)
      h2load -n "${concurrency}" -c 1 -m "${concurrency}" \
        --connect-to="127.0.0.1:${HTTPS_PORT}" -H 'accept-encoding: identity' \
        "${url}" >"${warmup_raw}" 2>&1 || true
      ;;
    h2-multi)
      local warm_clients=$((concurrency < 4 ? concurrency : 4))
      local warm_streams=$(((concurrency + warm_clients - 1) / warm_clients))
      h2load -n "${concurrency}" -c "${warm_clients}" -m "${warm_streams}" \
        --connect-to="127.0.0.1:${HTTPS_PORT}" -H 'accept-encoding: identity' \
        "${url}" >"${warmup_raw}" 2>&1 || true
      ;;
  esac

  : >"${resources}"
  sample_resources "${PROXY_PID}" "${resources}" &
  local sampler_pid=$!
  sleep 0.05
  local rc
  case "${protocol}" in
    h1-keepalive)
      wrk --latency -t1 -c "${concurrency}" -d "${DURATION}" \
        -s "${ROOT}/bench/wrk-keepalive.lua" -H 'Host: bench.test' \
        -H 'Accept-Encoding: identity' "${url}" >"${raw}" 2>&1
      rc=$?
      ;;
    h2-single)
      h2load -D "${DURATION}" --warm-up-time="${WARMUP}" -c 1 -m "${concurrency}" \
        --connect-to="127.0.0.1:${HTTPS_PORT}" -H 'accept-encoding: identity' \
        --log-file "${request_log}" "${url}" >"${raw}" 2>&1
      rc=$?
      ;;
    h2-multi)
      local clients=$((concurrency < 4 ? concurrency : 4))
      local streams=$(((concurrency + clients - 1) / clients))
      h2load -D "${DURATION}" --warm-up-time="${WARMUP}" \
        -c "${clients}" -m "${streams}" --connect-to="127.0.0.1:${HTTPS_PORT}" \
        -H 'accept-encoding: identity' --log-file "${request_log}" \
        "${url}" >"${raw}" 2>&1
      rc=$?
      ;;
  esac
  sleep 0.05
  kill "${sampler_pid}" >/dev/null 2>&1 || true
  wait "${sampler_pid}" >/dev/null 2>&1 || true

  local latency_line resource_line rps transport_errors http_errors errors distribution
  if [[ "${protocol}" == h2-* ]]; then
    latency_line=$(python3 "${ROOT}/bench/summarize_h2load.py" "${request_log}" 2>/dev/null || true)
    rps=$(sed -nE 's/.*finished in [^,]+, ([0-9.]+) req\/s.*/\1/p' "${raw}" | tail -1)
    transport_errors=$(sed -nE 's/requests: [0-9]+ total, [0-9]+ started, [0-9]+ done, [0-9]+ succeeded, ([0-9]+) failed.*/\1/p' "${raw}" | tail -1)
    http_errors=$(awk -F '\t' '$2 >= 100 && $2 <= 599 && $2 != 200 {count++} END {print count + 0}' "${request_log}" 2>/dev/null)
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
  local status=PASS
  if ((rc != 0 || errors != 0)) || [[ "${rps}" == 0 || "${rps}" == 0.00 ]]; then
    status=FAIL
    FAILURES=$((FAILURES + 1))
  fi
  printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
    "${candidate}" "${protocol}" "${size}" "${concurrency}" "${round}" \
    "${status}" "${rps}" "$(field "${latency_line}" p50)" \
    "$(field "${latency_line}" p90)" "$(field "${latency_line}" p95)" \
    "$(field "${latency_line}" p99)" "$(field "${latency_line}" p999)" \
    "$(field "${latency_line}" max)" "$(field "${resource_line}" cpu_avg)" \
    "$(field "${resource_line}" cpu_peak)" "$(field "${resource_line}" rss_avg_kib)" \
    "$(field "${resource_line}" rss_peak_kib)" "${errors}" "${distribution:-NA}" \
    "${raw}" >>"${OUTPUT}/results.tsv"
}

for ((round = 1; round <= ROUNDS; round++)); do
  if ((round % 2 == 1)); then
    ORDER=("${CANDIDATES[@]}")
  else
    ORDER=()
    for ((index = ${#CANDIDATES[@]} - 1; index >= 0; index--)); do
      ORDER+=("${CANDIDATES[index]}")
    done
  fi
  for candidate in "${ORDER[@]}"; do
    if ! start_candidate "${candidate}" "${round}"; then
      echo "${candidate} failed to start in round ${round}" >&2
      FAILURES=$((FAILURES + 1))
      cleanup_proxy
      continue
    fi
    sleep 0.5
    for protocol in "${PROTOCOLS[@]}"; do
      for size in "${PAYLOADS[@]}"; do
        for concurrency in "${CONCURRENCIES[@]}"; do
          run_case "${candidate}" "${protocol}" "${size}" "${concurrency}" "${round}"
        done
      done
    done
    cleanup_proxy
    sleep 0.5
  done
done

echo "results=${OUTPUT}/results.tsv failures=${FAILURES}"
exit $((FAILURES > 0))
