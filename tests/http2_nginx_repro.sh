#!/usr/bin/env bash
# Reproduce the reported path with the exact custom NGINX image:
# client -> Pingora TLS/H2 -> tae00217/jbs-nginx:ultra-4.0 HTTP/1.1.
set -uo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
RUNTIME=${PINGORA_NGINX_REPRO_RUNTIME:-/tmp/pingora-h2-nginx-repro}
IMAGE=${JBS_NGINX_IMAGE:-tae00217/jbs-nginx:ultra-4.0}
CONTAINER=pingora-h2-nginx-repro-$$
GATEWAY_PID=
FAILURES=0

cleanup() {
  if [[ -n "${GATEWAY_PID}" ]]; then
    kill "${GATEWAY_PID}" 2>/dev/null || true
    wait "${GATEWAY_PID}" 2>/dev/null || true
  fi
  docker rm -f "${CONTAINER}" >/dev/null 2>&1 || true
}
trap cleanup EXIT INT TERM

rm -rf "${RUNTIME}"
mkdir -p "${RUNTIME}"
BODY=0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ-_
[[ "${#BODY}" -eq 64 ]]
EXPECTED=$(printf '%s' "${BODY}" | sha256sum | awk '{print $1}')

cat >"${RUNTIME}/nginx.conf" <<EOF
worker_processes 1;
error_log /dev/stderr info;
pid /tmp/nginx.pid;
events { worker_connections 1024; }
http {
  access_log off;
  server {
    listen 127.0.0.1:19092;
    location = /fixed/64 {
      default_type application/octet-stream;
      add_header X-Backend jbs-nginx-ultra-4.0 always;
      return 200 '${BODY}';
    }
  }
}
EOF

docker run -d --name "${CONTAINER}" --network host \
  --read-only --tmpfs /tmp:rw,noexec,nosuid,size=8m \
  --tmpfs /etc/nginx/quic:rw,noexec,nosuid,size=1m \
  -v "${RUNTIME}/nginx.conf:/etc/nginx/nginx.conf:ro" \
  "${IMAGE}" >/dev/null

nginx_ready=0
for _ in {1..50}; do
  if curl --noproxy '*' -fsS http://127.0.0.1:19092/fixed/64 -o /dev/null 2>/dev/null; then
    nginx_ready=1
    break
  fi
  if ! docker inspect -f '{{.State.Running}}' "${CONTAINER}" 2>/dev/null | grep -qx true; then
    break
  fi
  sleep 0.1
done
if [[ "${nginx_ready}" -ne 1 ]]; then
  docker logs "${CONTAINER}" >"${RUNTIME}/nginx.stdout" 2>"${RUNTIME}/nginx.stderr" || true
  echo "custom NGINX backend failed to start; see ${RUNTIME}/nginx.stderr" >&2
  exit 1
fi

openssl req -x509 -newkey rsa:2048 -nodes -days 1 \
  -subj "/CN=matrix.test" -addext "subjectAltName=DNS:matrix.test" \
  -keyout "${RUNTIME}/key.pem" -out "${RUNTIME}/cert.pem" >/dev/null 2>&1
cat >"${RUNTIME}/pingora.yaml" <<EOF
server:
  http_listen: ["127.0.0.1:18082"]
  https_listen: ["127.0.0.1:18445"]
  certificate: ${RUNTIME}/cert.pem
  private_key: ${RUNTIME}/key.pem
  threads: 1
  upstream_keepalive_pool_size: 128
  max_retries: 1
  graceful_shutdown_timeout_seconds: 2
  static_cache_bytes: 1048576
  health_socket: ${RUNTIME}/health.sock
trusted_proxies: ["127.0.0.0/8"]
upstreams:
  nginx:
    address: "127.0.0.1:19092"
  adguard_dns_doh:
    address: "127.0.0.1:19092"
hosts:
  matrix:
    domains: ["matrix.test"]
    handler: adguard-dns
    upstream: nginx
EOF

RUST_LOG=debug "${ROOT}/target/debug/pingora" --config "${RUNTIME}/pingora.yaml" \
  >"${RUNTIME}/gateway.stdout" 2>"${RUNTIME}/gateway.stderr" &
GATEWAY_PID=$!
for _ in {1..100}; do
  if curl --noproxy '*' -fsS -H 'host: matrix.test' \
    http://127.0.0.1:18082/pingora-health -o /dev/null 2>/dev/null; then
    break
  fi
  sleep 0.1
done
kill -0 "${GATEWAY_PID}" 2>/dev/null || {
  echo "Pingora failed to start; see ${RUNTIME}/gateway.stderr" >&2
  exit 1
}

printf 'protocol\titeration\tstatus\tcurl_rc\tbytes\tsha256\n' \
  >"${RUNTIME}/results.tsv"
for protocol in h1 h2; do
  option=--http1.1
  [[ "${protocol}" == h2 ]] && option=--http2
  for iteration in $(seq 1 100); do
    body=${RUNTIME}/${protocol}-${iteration}.body
    error=${RUNTIME}/${protocol}-${iteration}.stderr
    rc=0
    curl --noproxy '*' -ksS "${option}" \
      --resolve matrix.test:18445:127.0.0.1 \
      https://matrix.test:18445/fixed/64 -o "${body}" 2>"${error}" || rc=$?
    size=$(stat -c '%s' "${body}" 2>/dev/null || echo 0)
    digest=$(sha256sum "${body}" 2>/dev/null | awk '{print $1}')
    status=pass
    if [[ "${rc}" -ne 0 || "${size}" -ne 64 || "${digest}" != "${EXPECTED}" ]]; then
      status=fail
      FAILURES=$((FAILURES + 1))
    fi
    printf '%s\t%s\t%s\t%s\t%s\t%s\n' \
      "${protocol}" "${iteration}" "${status}" "${rc}" "${size}" "${digest}" \
      >>"${RUNTIME}/results.tsv"
  done
done

nghttp --no-verify-peer -v -H ':authority: matrix.test' \
  https://127.0.0.1:18445/fixed/64 \
  https://127.0.0.1:18445/fixed/64 \
  >"${RUNTIME}/nghttp.stdout" 2>"${RUNTIME}/nghttp.stderr" || \
  FAILURES=$((FAILURES + 1))
h2load -n 3200 -c 1 -m 32 -H 'host: matrix.test' \
  https://127.0.0.1:18445/fixed/64 \
  >"${RUNTIME}/h2load.stdout" 2>"${RUNTIME}/h2load.stderr" || \
  FAILURES=$((FAILURES + 1))
if grep -Eq '([1-9][0-9]* failed|[1-9][0-9]* errored|[1-9][0-9]* timeout)' \
  "${RUNTIME}/h2load.stdout"; then
  FAILURES=$((FAILURES + 1))
fi

docker logs "${CONTAINER}" >"${RUNTIME}/nginx.stdout" 2>"${RUNTIME}/nginx.stderr" || true
failed_rows=$(awk -F '\t' '$3 == "fail" {count++} END {print count+0}' \
  "${RUNTIME}/results.tsv")
echo "custom NGINX upstream: image=${IMAGE}, sequential_failures=${failed_rows}, total_failures=${FAILURES}"
echo "raw logs: ${RUNTIME}"
[[ "${FAILURES}" -eq 0 ]]
