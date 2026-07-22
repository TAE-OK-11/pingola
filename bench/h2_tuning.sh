#!/usr/bin/env bash
set -uo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
IMAGE=${PINGORA_IMAGE:-ghcr.io/tae-ok-11/pingora:local}
OUTPUT=${H2_TUNING_OUTPUT:-${ROOT}/bench/results/h2-tuning-$(date -u +%Y%m%dT%H%M%SZ)}
BACKEND_PORT=${H2_BACKEND_PORT:-18900}
HTTPS_PORT=${H2_HTTPS_PORT:-443}
NAME=pingora-h2-tuning-$$
BACKEND_PID=
FAILURES=0

cleanup() {
  docker logs "${NAME}" >"${OUTPUT}/container-final.log" 2>&1 || true
  docker rm -f "${NAME}" >/dev/null 2>&1 || true
  if [[ -n "${BACKEND_PID}" ]]; then
    kill "${BACKEND_PID}" >/dev/null 2>&1 || true
    wait "${BACKEND_PID}" >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT INT TERM

mkdir -p "${OUTPUT}/raw"
chmod 0755 "${OUTPUT}" "${OUTPUT}/raw"
openssl req -x509 -newkey rsa:2048 -nodes -days 1 \
  -subj '/CN=h2.test' -addext 'subjectAltName=DNS:h2.test' \
  -keyout "${OUTPUT}/key.pem" -out "${OUTPUT}/cert.pem" >/dev/null 2>&1
chown 0:10001 "${OUTPUT}/key.pem" "${OUTPUT}/cert.pem"
chmod 0640 "${OUTPUT}/key.pem" "${OUTPUT}/cert.pem"

python3 "${ROOT}/bench/backend.py" --port "${BACKEND_PORT}" \
  >"${OUTPUT}/backend.stdout" 2>"${OUTPUT}/backend.stderr" &
BACKEND_PID=$!
for _ in {1..100}; do
  curl --noproxy '*' -fsS "http://127.0.0.1:${BACKEND_PORT}/bytes/64" -o /dev/null && break
  sleep 0.05
done

printf 'max_streams\trequested_streams\tpayload\tstatus\trps\tp50_us\tp95_us\tp99_us\tmax_us\terrors\tmemory_usage\traw\n' \
  >"${OUTPUT}/results.tsv"

field() {
  local line=$1 key=$2
  sed -nE "s/.*${key}=([^ ]+).*/\\1/p" <<<"${line}"
}

for maximum in 32 64 128 256; do
  docker rm -f "${NAME}" >/dev/null 2>&1 || true
  cat >"${OUTPUT}/pingora-${maximum}.yaml" <<EOF
server:
  http_listen: []
  https_listen: ["127.0.0.1:${HTTPS_PORT}"]
  certificate: /work/cert.pem
  private_key: /work/key.pem
  health_socket: /tmp/pingora/health.sock
  threads: 1
  upstream_keepalive_pool_size: 128
  max_retries: 0
  http2_max_concurrent_streams: ${maximum}
  graceful_shutdown_timeout_seconds: 2
trusted_proxies: ["127.0.0.0/8"]
upstreams:
  backend:
    address: "127.0.0.1:${BACKEND_PORT}"
hosts:
  benchmark:
    domains: ["h2.test"]
    handler: vaultwarden
    upstream: backend
route_limits:
  vaultwarden:
    rate_per_second: 0
    active_requests: 0
EOF
  chmod 0644 "${OUTPUT}/pingora-${maximum}.yaml"
  docker run --detach --name "${NAME}" --network host --read-only \
    --cap-drop ALL --cap-add NET_BIND_SERVICE --security-opt no-new-privileges \
    --tmpfs /tmp/pingora:rw,noexec,nosuid,nodev,uid=10001,gid=10001,mode=0700 \
    --volume "${OUTPUT}:/work:ro" --entrypoint /usr/local/bin/pingora "${IMAGE}" \
    --config "/work/pingora-${maximum}.yaml" >/dev/null
  for _ in {1..100}; do
    docker exec "${NAME}" /usr/local/bin/pingora \
      --config "/work/pingora-${maximum}.yaml" --healthcheck >/dev/null 2>&1 && break
    sleep 0.05
  done

  for payload in 64 4096; do
    expected=$(curl --noproxy '*' -fsS "http://127.0.0.1:${BACKEND_PORT}/bytes/${payload}" \
      | sha256sum | cut -d' ' -f1)
    actual=$(curl --noproxy '*' -ksS --http2 --resolve "h2.test:${HTTPS_PORT}:127.0.0.1" \
      "https://h2.test:${HTTPS_PORT}/bytes/${payload}" | sha256sum | cut -d' ' -f1)
    for streams in 32 64 128 256; do
      raw=${OUTPUT}/raw/max-${maximum}-streams-${streams}-bytes-${payload}.txt
      request_log=${raw%.txt}.requests.tsv
      h2load -n 500 -c 1 -m "${streams}" --sni h2.test -H 'host: h2.test' \
        --log-file "${request_log}" "https://127.0.0.1:${HTTPS_PORT}/bytes/${payload}" \
        >"${raw}" 2>&1
      rc=$?
      errors=$(sed -nE 's/requests: [0-9]+ total, [0-9]+ started, [0-9]+ done, [0-9]+ succeeded, ([0-9]+) failed.*/\1/p' \
        "${raw}" | tail -1)
      errors=${errors:-500}
      rps=$(sed -nE 's/.*finished in [^,]+, ([0-9.]+) req\/s.*/\1/p' "${raw}" | tail -1)
      latency=$(python3 "${ROOT}/bench/summarize_h2load.py" "${request_log}" 2>/dev/null || true)
      status=PASS
      if ((rc != 0 || errors != 0)) || [[ "${expected}" != "${actual}" ]]; then
        status=FAIL
        FAILURES=$((FAILURES + 1))
      fi
      memory=$(docker stats --no-stream --format '{{.MemUsage}}' "${NAME}" | tr -d ' ')
      printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
        "${maximum}" "${streams}" "${payload}" "${status}" "${rps:-0}" \
        "$(field "${latency}" p50)" "$(field "${latency}" p95)" \
        "$(field "${latency}" p99)" "$(field "${latency}" max)" "${errors}" \
        "${memory}" "${raw}" >>"${OUTPUT}/results.tsv"
    done
  done
  docker logs "${NAME}" >"${OUTPUT}/raw/max-${maximum}.container.log" 2>&1 || true
done

echo "H2 tuning results=${OUTPUT}/results.tsv failures=${FAILURES}"
exit $((FAILURES > 0))
