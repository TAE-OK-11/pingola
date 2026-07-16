#!/usr/bin/env bash
set -euo pipefail

PINGORA_BIN=${1:?usage: pgo_train.sh PINGORA_BIN BACKEND_BIN CLIENT_BIN OUTPUT_DIR}
BACKEND_BIN=${2:?usage: pgo_train.sh PINGORA_BIN BACKEND_BIN CLIENT_BIN OUTPUT_DIR}
CLIENT_BIN=${3:?usage: pgo_train.sh PINGORA_BIN BACKEND_BIN CLIENT_BIN OUTPUT_DIR}
OUTPUT_DIR=${4:?usage: pgo_train.sh PINGORA_BIN BACKEND_BIN CLIENT_BIN OUTPUT_DIR}
REQUIRE_PROFILE=${PGO_REQUIRE_PROFILE:-true}
HTTP_PORT=${PGO_HTTP_PORT:-19080}
HTTPS_PORT=${PGO_HTTPS_PORT:-19043}
BACKEND_PORT=${PGO_BACKEND_PORT:-19000}
RUNTIME_DIR=${OUTPUT_DIR}/runtime
STATIC_DIR=${RUNTIME_DIR}/static
BACKEND_PID=
PINGORA_PID=

cleanup() {
  if [[ -n "${PINGORA_PID}" ]]; then
    kill -TERM "${PINGORA_PID}" >/dev/null 2>&1 || true
    wait "${PINGORA_PID}" >/dev/null 2>&1 || true
  fi
  if [[ -n "${BACKEND_PID}" ]]; then
    kill -TERM "${BACKEND_PID}" >/dev/null 2>&1 || true
    wait "${BACKEND_PID}" >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT INT TERM

install -d -m 0700 "${OUTPUT_DIR}" "${RUNTIME_DIR}" "${STATIC_DIR}"
openssl req -x509 -newkey rsa:2048 -nodes -days 1 \
  -subj '/CN=pgo.test' -addext 'subjectAltName=DNS:pgo.test,DNS:static.test,DNS:music.test' \
  -keyout "${RUNTIME_DIR}/key.pem" -out "${RUNTIME_DIR}/cert.pem" \
  >"${OUTPUT_DIR}/openssl-cert.log" 2>&1
chmod 0600 "${RUNTIME_DIR}/key.pem" "${RUNTIME_DIR}/cert.pem"
dd if=/dev/zero of="${STATIC_DIR}/hot.bin" bs=4096 count=1 status=none
for index in $(seq 1 64); do
  dd if=/dev/zero of="${STATIC_DIR}/cold-${index}.bin" bs=512 count=1 status=none
done
cat >"${OUTPUT_DIR}/pingora.yaml" <<EOF
server:
  http_listen: ["127.0.0.1:${HTTP_PORT}"]
  https_listen: ["127.0.0.1:${HTTPS_PORT}"]
  certificate: ${RUNTIME_DIR}/cert.pem
  private_key: ${RUNTIME_DIR}/key.pem
  health_socket: ${RUNTIME_DIR}/health.sock
  threads: 1
  upstream_keepalive_pool_size: 128
  downstream_keepalive_requests: 1000000
  max_retries: 0
  access_log: false
  http2_max_concurrent_streams: 128
  static_cache_bytes: 1048576
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
  api:
    domains: ["pgo.test"]
    handler: vaultwarden
    upstream: backend
  music:
    domains: ["music.test"]
    handler: navidrome-main
    upstream: backend
  static:
    domains: ["static.test"]
    handler: static
    static_root: ${STATIC_DIR}
route_limits:
  vaultwarden:
    rate_per_second: 0
    active_requests: 0
  navidrome_api:
    rate_per_second: 0
    active_requests: 0
  navidrome_stream:
    rate_per_second: 0
    active_requests: 0
EOF

"${BACKEND_BIN}" --port "${BACKEND_PORT}" >"${OUTPUT_DIR}/backend.log" 2>&1 &
BACKEND_PID=$!
for _ in {1..100}; do
  if (exec 3<>"/dev/tcp/127.0.0.1/${BACKEND_PORT}") 2>/dev/null; then
    exec 3>&-
    break
  fi
  sleep 0.05
done
kill -0 "${BACKEND_PID}"

LLVM_PROFILE_FILE="${OUTPUT_DIR}/pingora-%p-%m.profraw" \
  "${PINGORA_BIN}" --config "${OUTPUT_DIR}/pingora.yaml" \
  >"${OUTPUT_DIR}/pingora.log" 2>&1 &
PINGORA_PID=$!
pingora_ready=false
for _ in {1..100}; do
  if (exec 3<>"/dev/tcp/127.0.0.1/${HTTP_PORT}") 2>/dev/null; then
    exec 3>&-
    pingora_ready=true
    break
  fi
  if ! kill -0 "${PINGORA_PID}" 2>/dev/null; then
    echo "instrumented Pingora exited before readiness" >&2
    sed -n '1,160p' "${OUTPUT_DIR}/pingora.log" >&2
    exit 1
  fi
  sleep 0.05
done
if [[ "${pingora_ready}" != true ]]; then
  echo "instrumented Pingora did not become ready on 127.0.0.1:${HTTP_PORT}" >&2
  exit 1
fi

"${CLIENT_BIN}" --port "${HTTP_PORT}" --threads 1 --requests-per-thread 5000
"${CLIENT_BIN}" --port "${HTTP_PORT}" --threads 8 --requests-per-thread 6000

# Small/large JSON API responses on persistent downstream and upstream sockets.
"${CLIENT_BIN}" --port "${HTTP_PORT}" --threads 4 --requests-per-thread 3000 \
  --path /json/512 --expected-length 512 --body-validation any
"${CLIENT_BIN}" --port "${HTTP_PORT}" --threads 2 --requests-per-thread 500 \
  --path /json/65536 --expected-length 65536 --body-validation any

# Static cold misses followed by a hot-cache keepalive workload.
for index in $(seq 1 64); do
  curl --noproxy '*' --fail --silent --show-error \
    -H 'Host: static.test' -H 'Accept-Encoding: identity' \
    "http://127.0.0.1:${HTTP_PORT}/cold-${index}.bin" -o /dev/null
done
"${CLIENT_BIN}" --port "${HTTP_PORT}" --threads 4 --requests-per-thread 2000 \
  --host static.test --path /hot.bin --expected-length 4096 --body-validation any

# Exercise expected proxy error responses without treating them as training failures.
"${CLIENT_BIN}" --port "${HTTP_PORT}" --threads 2 --requests-per-thread 250 \
  --path /missing --expected-status 404 --expected-length 10 --body-validation any
"${CLIENT_BIN}" --port "${HTTP_PORT}" --threads 2 --requests-per-thread 250 \
  --path /status/500 --expected-status 500 --expected-length 14 --body-validation any

# Navidrome-class streaming paths remain bounded and are consumed completely.
for _ in $(seq 1 8); do
  curl --noproxy '*' --fail --silent --show-error -H 'Host: music.test' \
    -H 'Accept-Encoding: identity' \
    "http://127.0.0.1:${HTTP_PORT}/stream/1048576" -o /dev/null
done
for _ in $(seq 1 2); do
  curl --noproxy '*' --fail --silent --show-error -H 'Host: music.test' \
    -H 'Accept-Encoding: identity' \
    "http://127.0.0.1:${HTTP_PORT}/stream/10485760" -o /dev/null
done

# HTTP/2 multiplexing over TLS for proxy and static routes. h2load exits zero on
# transport completion, so also require every request to succeed.
h2load -n 12000 -c 4 -m 32 --sni pgo.test \
  -H 'host: pgo.test' -H 'accept-encoding: identity' \
  "https://127.0.0.1:${HTTPS_PORT}/json/512" >"${OUTPUT_DIR}/h2-api.log" 2>&1
grep -Eq '0 failed, 0 errored' "${OUTPUT_DIR}/h2-api.log"
h2load -n 8000 -c 4 -m 32 --sni static.test \
  -H 'host: static.test' -H 'accept-encoding: identity' \
  "https://127.0.0.1:${HTTPS_PORT}/hot.bin" >"${OUTPUT_DIR}/h2-static.log" 2>&1
grep -Eq '0 failed, 0 errored' "${OUTPUT_DIR}/h2-static.log"

# Fresh TLS 1.3 connections and explicit session resumption. Each curl process
# creates a new connection. Every OpenSSL resumed connection rotates to the new
# single-use ticket and must report Reused instead of claiming false coverage.
for _ in $(seq 1 100); do
  curl --noproxy '*' --http1.1 --tlsv1.3 --tls-max 1.3 --insecure \
    --fail --silent --show-error -H 'Host: pgo.test' \
    "https://127.0.0.1:${HTTPS_PORT}/json/512" -o /dev/null
done
set +e
{ printf 'GET /json/512 HTTP/1.1\r\nHost: pgo.test\r\nConnection: keep-alive\r\n\r\n'; sleep 2; } \
  | timeout 3 openssl s_client -connect "127.0.0.1:${HTTPS_PORT}" \
      -servername pgo.test -tls1_3 -ign_eof \
      -sess_out "${OUTPUT_DIR}/tls-session.pem" \
      >"${OUTPUT_DIR}/tls-session-new.log" 2>&1
session_rc=$?
set -e
if [[ ${session_rc} -ne 0 && ${session_rc} -ne 124 ]]; then
  echo "failed to obtain a TLS 1.3 session: exit=${session_rc}" >&2
  exit 1
fi
test -s "${OUTPUT_DIR}/tls-session.pem"
: >"${OUTPUT_DIR}/tls-resumption.log"
for _ in $(seq 1 50); do
  { printf 'GET /json/512 HTTP/1.1\r\nHost: pgo.test\r\nConnection: keep-alive\r\n\r\n'; sleep 0.1; } \
    | openssl s_client -connect "127.0.0.1:${HTTPS_PORT}" -servername pgo.test \
        -tls1_3 -sess_in "${OUTPUT_DIR}/tls-session.pem" \
        -sess_out "${OUTPUT_DIR}/tls-session-next.pem" \
        >>"${OUTPUT_DIR}/tls-resumption.log" 2>&1
  mv "${OUTPUT_DIR}/tls-session-next.pem" "${OUTPUT_DIR}/tls-session.pem"
done
[[ $(grep -c '^Reused, TLSv1.3' "${OUTPUT_DIR}/tls-resumption.log") -eq 50 ]]

upstream_connections=$(curl --noproxy '*' --fail --silent --show-error \
  "http://127.0.0.1:${BACKEND_PORT}/stats/connections")
if [[ ! "${upstream_connections}" =~ ^[0-9]+$ ]] || ((upstream_connections >= 2048)); then
  echo "upstream keepalive reuse check failed: backend_connections=${upstream_connections}" >&2
  exit 1
fi

cat >"${OUTPUT_DIR}/workload.txt" <<EOF
h1_keepalive_mixed=53000
json_small=12000
json_large=1000
static_cold_miss=64
static_hot_hit=8000
expected_404=500
expected_500=500
stream_1mib=8
stream_10mib=2
h2_api=12000
h2_static=8000
tls_fresh=100
tls_resumption=50_verified
upstream_keepalive=verified
backend_connections=${upstream_connections}
EOF

kill -TERM "${PINGORA_PID}"
wait "${PINGORA_PID}"
PINGORA_PID=
if [[ "${REQUIRE_PROFILE}" == true ]]; then
  compgen -G "${OUTPUT_DIR}/*.profraw" >/dev/null
elif [[ "${REQUIRE_PROFILE}" != false ]]; then
  echo "PGO_REQUIRE_PROFILE must be true or false" >&2
  exit 2
fi
