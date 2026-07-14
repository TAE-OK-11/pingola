#!/usr/bin/env bash
set -euo pipefail

BINARY=${1:?instrumented Pingora binary is required}
PROFILE=${2:?BOLT profile path is required}
RUNTIME=$(mktemp -d /tmp/pingora-bolt-training.XXXXXX)
BACKEND_PID=
PINGORA_PID=

cleanup() {
  if [[ -n "${PINGORA_PID}" ]]; then
    kill -TERM "${PINGORA_PID}" 2>/dev/null || true
    wait "${PINGORA_PID}" 2>/dev/null || true
  fi
  if [[ -n "${BACKEND_PID}" ]]; then
    kill -TERM "${BACKEND_PID}" 2>/dev/null || true
    wait "${BACKEND_PID}" 2>/dev/null || true
  fi
  rm -rf "${RUNTIME}"
}
trap cleanup EXIT

mkdir -p "${RUNTIME}/www"
printf '%s\n' 'pingora-bolt-static-training' >"${RUNTIME}/www/index.html"
openssl req -x509 -newkey rsa:2048 -nodes -days 1 \
  -subj '/CN=bench.test' -addext 'subjectAltName=DNS:bench.test' \
  -keyout "${RUNTIME}/key.pem" -out "${RUNTIME}/cert.pem" >/dev/null 2>&1

cat >"${RUNTIME}/pingora.yaml" <<EOF
server:
  http_listen: ["127.0.0.1:18780"]
  https_listen: ["127.0.0.1:18743"]
  certificate: ${RUNTIME}/cert.pem
  private_key: ${RUNTIME}/key.pem
  health_socket: ${RUNTIME}/health.sock
  threads: 1
  upstream_keepalive_pool_size: 128
  max_retries: 0
  access_log: false
  health_details: false
  http2_max_concurrent_streams: 128
  graceful_shutdown_timeout_seconds: 1
trusted_proxies: ["127.0.0.0/8"]
upstreams:
  backend:
    address: "127.0.0.1:18700"
    connect_timeout_seconds: 2
    read_timeout_seconds: 30
    write_timeout_seconds: 30
hosts:
  bench:
    domains: ["bench.test"]
    handler: navidrome-main
    upstream: backend
    max_body_bytes: 536870912
  static:
    domains: ["static.test"]
    handler: static
    static_root: ${RUNTIME}/www
route_limits:
  navidrome_api: { rate_per_second: 0, active_requests: 0 }
  navidrome_stream: { rate_per_second: 0, active_requests: 0 }
EOF

python3 bench/backend.py --port 18700 >"${RUNTIME}/backend.log" 2>&1 &
BACKEND_PID=$!
RUST_LOG=off "${BINARY}" --config "${RUNTIME}/pingora.yaml" \
  >"${RUNTIME}/pingora.log" 2>&1 &
PINGORA_PID=$!

for _ in {1..100}; do
  if curl --noproxy '*' -fsS -H 'host: invalid.test' \
    http://127.0.0.1:18780/pingora-health -o /dev/null 2>/dev/null; then
    break
  fi
  kill -0 "${PINGORA_PID}"
  sleep 0.1
done
curl --noproxy '*' -fsS -H 'host: invalid.test' \
  http://127.0.0.1:18780/pingora-health -o /dev/null

# Weight the profile toward the small HTTP/1.1 proxy path where the current
# binary trails NGINX, while retaining HTTP/2, static, Range and streaming paths.
h2load --h1 -n 4000 -c 8 -m 1 -H 'host: bench.test' \
  http://127.0.0.1:18780/bytes/64 >/dev/null
h2load --h1 -n 3000 -c 8 -m 1 -H 'host: bench.test' \
  http://127.0.0.1:18780/bytes/4096 >/dev/null
h2load -n 3000 -c 4 -m 32 -H 'host: bench.test' \
  https://127.0.0.1:18743/bytes/64 >/dev/null
h2load -n 2000 -c 4 -m 32 -H 'host: bench.test' \
  https://127.0.0.1:18743/bytes/4096 >/dev/null

curl --noproxy '*' -kfsS -H 'host: bench.test' -H 'range: bytes=0-4095' \
  https://127.0.0.1:18743/bytes/1048576 -o /dev/null
curl --noproxy '*' -kfsS -H 'host: bench.test' \
  https://127.0.0.1:18743/stream/1048576 -o /dev/null
curl --noproxy '*' -fsS -H 'host: static.test' \
  http://127.0.0.1:18780/ -o /dev/null

# The instrumentation thread writes cumulative data once per interval. Wait for
# a completed post-training dump instead of relying on signal-handler timing.
sleep 2
test -s "${PROFILE}"
kill -TERM "${PINGORA_PID}"
wait "${PINGORA_PID}" || true
PINGORA_PID=
