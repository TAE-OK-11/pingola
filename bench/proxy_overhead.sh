#!/usr/bin/env bash
set -uo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
IMAGE=${PINGORA_IMAGE:-ghcr.io/tae-ok-11/pingora:local}
OUTPUT=${OVERHEAD_OUTPUT:-${ROOT}/bench/results/overhead-$(date -u +%Y%m%dT%H%M%SZ)}
ROUNDS=${OVERHEAD_ROUNDS:-3}
DURATION=${OVERHEAD_DURATION:-10s}
WARMUP=${OVERHEAD_WARMUP:-2s}
CPUS=${OVERHEAD_CPUS:-2}
MEMORY=${OVERHEAD_MEMORY:-1g}
WORKERS=${OVERHEAD_WORKERS:-2}
HANDLER=${OVERHEAD_HANDLER:-vaultwarden}
PATH_UNDER_TEST=${OVERHEAD_PATH:-/bytes/64}
ACTIVE_LIMIT=${OVERHEAD_ACTIVE_LIMIT:-0}
BACKEND_PORT=${OVERHEAD_BACKEND_PORT:-18900}
PROXY_PORT=${OVERHEAD_PROXY_PORT:-18980}
NAME=pingora-overhead-$$
BACKEND_PID=

case "${HANDLER}" in
  navidrome-main|navidrome-cdn|vaultwarden|couchdb|adguard-dns|adguard-korea) ;;
  *)
    echo "unsupported OVERHEAD_HANDLER=${HANDLER}" >&2
    exit 2
    ;;
esac
if [[ ! "${ACTIVE_LIMIT}" =~ ^[0-9]+$ ]]; then
  echo "OVERHEAD_ACTIVE_LIMIT must be a non-negative integer" >&2
  exit 2
fi
if [[ "${PATH_UNDER_TEST}" != /* ]] || [[ "${PATH_UNDER_TEST}" == *$'\r'* || "${PATH_UNDER_TEST}" == *$'\n'* ]]; then
  echo "OVERHEAD_PATH must be a safe absolute request path" >&2
  exit 2
fi
if [[ "${OUTPUT}" != /* ]]; then
  OUTPUT=${ROOT}/${OUTPUT}
fi

cleanup() {
  docker rm -f "${NAME}" >/dev/null 2>&1 || true
  if [[ -n "${BACKEND_PID}" ]]; then
    kill "${BACKEND_PID}" >/dev/null 2>&1 || true
    wait "${BACKEND_PID}" >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT INT TERM

for command in docker curl wrk rustc sha256sum python3; do
  if ! command -v "${command}" >/dev/null; then
    echo "missing required command: ${command}" >&2
    exit 2
  fi
done

mkdir -p "${OUTPUT}/raw"
chmod 0755 "${OUTPUT}" "${OUTPUT}/raw"
BACKEND_BIN=${OUTPUT}/backend-rust
rustc --edition=2021 -D warnings -C opt-level=3 -C codegen-units=1 -C panic=abort \
  -C target-cpu=native -C strip=symbols "${ROOT}/bench/backend.rs" -o "${BACKEND_BIN}" \
  >"${OUTPUT}/backend-build.stdout" 2>"${OUTPUT}/backend-build.stderr"

cat >"${OUTPUT}/pingora.yaml" <<EOF
server:
  http_listen: ["127.0.0.1:${PROXY_PORT}"]
  https_listen: []
  health_socket: /tmp/pingora/health.sock
  threads: ${WORKERS}
  upstream_keepalive_pool_size: 128
  downstream_keepalive_requests: 1000000
  max_retries: 0
  access_log: false
  global_active_requests: 0
trusted_proxies: ["127.0.0.0/8"]
upstreams:
  backend:
    address: "127.0.0.1:${BACKEND_PORT}"
    protocol: http1
    connect_timeout_seconds: 2
    read_timeout_seconds: 3600
    write_timeout_seconds: 3600
    idle_timeout_seconds: 30
  adguard_dns_doh:
    address: "127.0.0.1:${BACKEND_PORT}"
    protocol: http1
  adguard_korea_doh:
    address: "127.0.0.1:${BACKEND_PORT}"
    protocol: http1
hosts:
  overhead:
    domains: ["overhead.test"]
    handler: ${HANDLER}
    upstream: backend
    max_body_bytes: 536870912
route_limits:
  navidrome_stream: { rate_per_second: 0, active_requests: ${ACTIVE_LIMIT} }
  navidrome_cover: { rate_per_second: 0, active_requests: ${ACTIVE_LIMIT} }
  navidrome_api: { rate_per_second: 0, active_requests: ${ACTIVE_LIMIT} }
  vaultwarden_auth: { rate_per_second: 0, active_requests: ${ACTIVE_LIMIT} }
  vaultwarden_hub: { rate_per_second: 0, active_requests: ${ACTIVE_LIMIT} }
  vaultwarden: { rate_per_second: 0, active_requests: ${ACTIVE_LIMIT} }
  couchdb: { rate_per_second: 0, active_requests: ${ACTIVE_LIMIT} }
  doh: { rate_per_second: 0, active_requests: ${ACTIVE_LIMIT} }
  adguard_ui: { rate_per_second: 0, active_requests: ${ACTIVE_LIMIT} }
EOF
chmod 0644 "${OUTPUT}/pingora.yaml"

"${BACKEND_BIN}" --port "${BACKEND_PORT}" \
  >"${OUTPUT}/backend.stdout" 2>"${OUTPUT}/backend.stderr" &
BACKEND_PID=$!
ready=false
for _ in {1..100}; do
  if curl --noproxy '*' -fsS "http://127.0.0.1:${BACKEND_PORT}/health" -o /dev/null 2>/dev/null; then
    ready=true
    break
  fi
  sleep 0.05
done
if [[ "${ready}" != true ]]; then
  echo "overhead backend failed readiness" >&2
  exit 1
fi

docker run --detach --name "${NAME}" --network host --read-only \
  --cpus "${CPUS}" --memory "${MEMORY}" --memory-swap "${MEMORY}" \
  --ulimit nofile=32768:32768 --cap-drop ALL --cap-add NET_BIND_SERVICE \
  --security-opt no-new-privileges \
  --tmpfs /tmp/pingora:rw,noexec,nosuid,nodev,uid=10001,gid=10001,mode=0700 \
  --volume "${OUTPUT}:/work:ro" --entrypoint /usr/local/bin/pingora \
  "${IMAGE}" --config /work/pingora.yaml >/dev/null
ready=false
for _ in {1..100}; do
  if curl --noproxy '*' -fsS -H 'host: overhead.test' \
    "http://127.0.0.1:${PROXY_PORT}${PATH_UNDER_TEST}" -o /dev/null 2>/dev/null; then
    ready=true
    break
  fi
  sleep 0.05
done
if [[ "${ready}" != true ]]; then
  docker logs "${NAME}" >&2
  exit 1
fi

DIRECT_URL="http://127.0.0.1:${BACKEND_PORT}${PATH_UNDER_TEST}"
PROXY_URL="http://127.0.0.1:${PROXY_PORT}${PATH_UNDER_TEST}"
DIRECT_SHA=$(curl --noproxy '*' -fsS "${DIRECT_URL}" | sha256sum | cut -d' ' -f1)
PROXY_SHA=$(curl --noproxy '*' -fsS -H 'host: overhead.test' "${PROXY_URL}" | sha256sum | cut -d' ' -f1)
if [[ "${DIRECT_SHA}" != "${PROXY_SHA}" ]]; then
  echo "proxy body mismatch: direct=${DIRECT_SHA} proxy=${PROXY_SHA}" >&2
  exit 1
fi

cat >"${OUTPUT}/environment.txt" <<EOF
timestamp=$(date -u +%FT%TZ)
image=${IMAGE}
handler=${HANDLER}
path=${PATH_UNDER_TEST}
rounds=${ROUNDS}
duration=${DURATION}
warmup=${WARMUP}
cpus=${CPUS}
memory=${MEMORY}
workers=${WORKERS}
active_limit=${ACTIVE_LIMIT}
body_sha256=${DIRECT_SHA}
note=direct backend and proxied requests alternate on the same host; load generator shares host CPUs
EOF
docker image inspect "${IMAGE}" >"${OUTPUT}/image-inspect.json"
printf 'target\tconcurrency\tround\tstatus\trps\tp99_us\terrors\traw\n' >"${OUTPUT}/results.tsv"

run_case() {
  local target=$1 concurrency=$2 round=$3 url raw warm rc rps p99 errors
  if [[ "${target}" == direct ]]; then
    url=${DIRECT_URL}
  else
    url=${PROXY_URL}
  fi
  raw=${OUTPUT}/raw/${target}-r${round}-c${concurrency}.txt
  warm=${OUTPUT}/raw/${target}-r${round}-c${concurrency}.warmup.txt
  wrk -t1 -c "${concurrency}" -d "${WARMUP}" -s "${ROOT}/bench/wrk-keepalive.lua" \
    -H 'Host: overhead.test' -H 'Accept-Encoding: identity' "${url}" >"${warm}" 2>&1 || true
  wrk --latency -t1 -c "${concurrency}" -d "${DURATION}" \
    -s "${ROOT}/bench/wrk-keepalive.lua" -H 'Host: overhead.test' \
    -H 'Accept-Encoding: identity' "${url}" >"${raw}" 2>&1
  rc=$?
  rps=$(awk '/Requests\/sec:/ {print $2}' "${raw}" | tail -1)
  p99=$(sed -nE 's/.*LATENCY_US .*p99=([0-9]+).*/\1/p' "${raw}" | tail -1)
  errors=$(awk '/Socket errors:/ {gsub(/[^0-9 ]/, ""); print $1+$2+$3+$4}' "${raw}" | tail -1)
  errors=${errors:-0}
  status=PASS
  if ((rc != 0 || errors != 0)) || [[ -z "${rps}" || "${rps}" == 0 || "${rps}" == 0.00 ]]; then
    status=FAIL
  fi
  printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
    "${target}" "${concurrency}" "${round}" "${status}" "${rps:-0}" "${p99:-NA}" \
    "${errors}" "${raw}" >>"${OUTPUT}/results.tsv"
}

for ((round = 1; round <= ROUNDS; round++)); do
  if ((round % 2 == 1)); then ORDER=(direct proxy); else ORDER=(proxy direct); fi
  for target in "${ORDER[@]}"; do
    for concurrency in 1 8 32; do
      run_case "${target}" "${concurrency}" "${round}"
    done
  done
done

python3 - "${OUTPUT}/results.tsv" >"${OUTPUT}/summary.tsv" <<'PY'
import csv, statistics, sys
rows = list(csv.DictReader(open(sys.argv[1]), delimiter="\t"))
print("concurrency\tdirect_rps\tproxy_rps\tproxy_overhead_pct\tdirect_p99_us\tproxy_p99_us\tp99_overhead_pct")
for concurrency in ("1", "8", "32"):
    selected = [r for r in rows if r["concurrency"] == concurrency and r["status"] == "PASS"]
    values = {}
    for target in ("direct", "proxy"):
        target_rows = [r for r in selected if r["target"] == target]
        values[target] = (
            statistics.median(float(r["rps"]) for r in target_rows),
            statistics.median(float(r["p99_us"]) for r in target_rows),
        )
    direct_rps, direct_p99 = values["direct"]
    proxy_rps, proxy_p99 = values["proxy"]
    print(
        concurrency,
        f"{direct_rps:.2f}", f"{proxy_rps:.2f}", f"{(proxy_rps / direct_rps - 1) * 100:.2f}",
        f"{direct_p99:.0f}", f"{proxy_p99:.0f}", f"{(proxy_p99 / direct_p99 - 1) * 100:.2f}",
        sep="\t",
    )
PY

failures=$(awk -F '\t' 'NR > 1 && $4 != "PASS" {count++} END {print count + 0}' "${OUTPUT}/results.tsv")
cat "${OUTPUT}/summary.tsv"
echo "results=${OUTPUT} failures=${failures}"
((failures == 0))
