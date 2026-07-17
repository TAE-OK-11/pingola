#!/usr/bin/env bash
set -euo pipefail

PINGORA_BIN=${1:?usage: pgo_train.sh PINGORA_BIN BACKEND_BIN CLIENT_BIN OUTPUT_DIR SCENARIO}
BACKEND_BIN=${2:?usage: pgo_train.sh PINGORA_BIN BACKEND_BIN CLIENT_BIN OUTPUT_DIR SCENARIO}
CLIENT_BIN=${3:?usage: pgo_train.sh PINGORA_BIN BACKEND_BIN CLIENT_BIN OUTPUT_DIR SCENARIO}
OUTPUT_DIR=${4:?usage: pgo_train.sh PINGORA_BIN BACKEND_BIN CLIENT_BIN OUTPUT_DIR SCENARIO}
SCENARIO=${5:?usage: pgo_train.sh PINGORA_BIN BACKEND_BIN CLIENT_BIN OUTPUT_DIR SCENARIO}

ECDSA_CURVE=${PGO_ECDSA_CURVE:-prime256v1}
REQUIRE_PROFILE=${PGO_REQUIRE_PROFILE:-true}
HTTP_PORT=${PGO_HTTP_PORT:-19080}
HTTPS_PORT=${PGO_HTTPS_PORT:-19043}
BACKEND_PORT=${PGO_BACKEND_PORT:-19000}
RUNTIME_DIR=${OUTPUT_DIR}/runtime
STATIC_DIR=${RUNTIME_DIR}/static
BACKEND_PID=
PINGORA_PID=

case "${SCENARIO}" in
  h1|h2|tls|tail) ;;
  *)
    echo "unsupported PGO scenario: ${SCENARIO}" >&2
    exit 2
    ;;
esac

case "${ECDSA_CURVE}" in
  prime256v1|secp384r1) ;;
  *)
    echo "unsupported ECDSA curve: ${ECDSA_CURVE}" >&2
    exit 2
    ;;
esac

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

run_h2load() {
  local name=$1
  shift
  h2load "$@" >"${OUTPUT_DIR}/${name}.log" 2>&1
  if ! grep -Eq '0 failed, 0 errored' "${OUTPUT_DIR}/${name}.log"; then
    echo "h2load workload failed: ${name}" >&2
    sed -n '1,180p' "${OUTPUT_DIR}/${name}.log" >&2
    exit 1
  fi
}

wait_for_tcp() {
  local port=$1
  local process_pid=$2
  local name=$3

  for _ in {1..160}; do
    if (exec 3<>"/dev/tcp/127.0.0.1/${port}") 2>/dev/null; then
      exec 3>&-
      return 0
    fi
    if ! kill -0 "${process_pid}" 2>/dev/null; then
      echo "${name} exited before readiness" >&2
      return 1
    fi
    sleep 0.05
  done

  echo "${name} did not become ready on 127.0.0.1:${port}" >&2
  return 1
}

train_h1() {
  # Persistent HTTP/1.1 fast paths at low and moderate concurrency.
  "${CLIENT_BIN}" \
    --port "${HTTP_PORT}" \
    --threads 1 \
    --requests-per-thread 8000

  "${CLIENT_BIN}" \
    --port "${HTTP_PORT}" \
    --threads 8 \
    --requests-per-thread 5000

  # Typical API payload sizes.
  "${CLIENT_BIN}" \
    --port "${HTTP_PORT}" \
    --threads 4 \
    --requests-per-thread 3500 \
    --path /json/512 \
    --expected-length 512 \
    --body-validation any

  "${CLIENT_BIN}" \
    --port "${HTTP_PORT}" \
    --threads 2 \
    --requests-per-thread 750 \
    --path /json/65536 \
    --expected-length 65536 \
    --body-validation any

  # New-connection churn, rather than training only one eternal keepalive socket.
  for _ in $(seq 1 24); do
    "${CLIENT_BIN}" \
      --port "${HTTP_PORT}" \
      --threads 16 \
      --requests-per-thread 16
  done

  # HTTP/1.1 over TLS with one request in flight per connection.
  run_h2load h1-tls \
    --h1 \
    -n 12000 \
    -c 16 \
    -m 1 \
    --sni pgo.test \
    -H 'host: pgo.test' \
    -H 'accept-encoding: identity' \
    "https://127.0.0.1:${HTTPS_PORT}/json/512"

  # POST request bodies, byte ranges and chunked upstream responses.
  for _ in $(seq 1 400); do
    printf '\x00\x01\x00\x00\x00\x01\x00\x00\x00\x00\x00\x00' \
      | curl \
          --noproxy '*' \
          --http1.1 \
          --silent \
          --show-error \
          --output /dev/null \
          -H 'Host: pgo.test' \
          -H 'Content-Type: application/dns-message' \
          --data-binary @- \
          "http://127.0.0.1:${HTTP_PORT}/dns-query"
  done

  for _ in $(seq 1 500); do
    curl \
      --noproxy '*' \
      --http1.1 \
      --fail \
      --silent \
      --show-error \
      --output /dev/null \
      -H 'Host: pgo.test' \
      -H 'Range: bytes=4096-8191' \
      "http://127.0.0.1:${HTTP_PORT}/bytes/1048576"
  done

  for _ in $(seq 1 24); do
    curl \
      --noproxy '*' \
      --http1.1 \
      --fail \
      --silent \
      --show-error \
      --output /dev/null \
      -H 'Host: music.test' \
      -H 'Accept-Encoding: identity' \
      "http://127.0.0.1:${HTTP_PORT}/stream/1048576"
  done
}

train_h2() {
  # Low-concurrency H2 latency path.
  run_h2load h2-low \
    -n 6000 \
    -c 1 \
    -m 1 \
    -w 16 \
    -W 16 \
    --sni pgo.test \
    -H 'host: pgo.test' \
    -H 'accept-encoding: identity' \
    "https://127.0.0.1:${HTTPS_PORT}/json/512"

  # Representative multiplexing level.
  run_h2load h2-normal \
    -n 24000 \
    -c 4 \
    -m 16 \
    -w 16 \
    -W 20 \
    --sni pgo.test \
    -H 'host: pgo.test' \
    -H 'accept-encoding: identity' \
    -H 'authorization: Bearer pgo-training-token' \
    -H 'cookie: session=pgo; preferences=h2' \
    "https://127.0.0.1:${HTTPS_PORT}/json/512"

  # High concurrency to train stream scheduling, backpressure and wakeups.
  run_h2load h2-high \
    -n 32000 \
    -c 16 \
    -m 64 \
    -w 16 \
    -W 20 \
    --sni pgo.test \
    -H 'host: pgo.test' \
    -H 'accept-encoding: identity' \
    "https://127.0.0.1:${HTTPS_PORT}/bytes/4096"

  # Large DATA frames and upstream chunked-to-H2 forwarding.
  run_h2load h2-large-json \
    -n 4000 \
    -c 4 \
    -m 8 \
    -w 16 \
    -W 20 \
    --sni pgo.test \
    -H 'host: pgo.test' \
    -H 'accept-encoding: identity' \
    "https://127.0.0.1:${HTTPS_PORT}/json/65536"

  run_h2load h2-stream \
    -n 256 \
    -c 4 \
    -m 4 \
    -w 16 \
    -W 20 \
    --sni music.test \
    -H 'host: music.test' \
    -H 'accept-encoding: identity' \
    "https://127.0.0.1:${HTTPS_PORT}/stream/1048576"

  # H2 static-cache hit path.
  run_h2load h2-static \
    -n 12000 \
    -c 4 \
    -m 32 \
    -w 16 \
    -W 20 \
    --sni static.test \
    -H 'host: static.test' \
    -H 'accept-encoding: identity' \
    "https://127.0.0.1:${HTTPS_PORT}/hot.bin"
}

train_tls() {
  # Thousands of full TLS 1.3 handshakes for both ALPN outcomes.
  for index in $(seq 1 8); do
    run_h2load "tls-fresh-h1-${index}" \
      --h1 \
      -n 128 \
      -c 128 \
      -m 1 \
      --sni pgo.test \
      -H 'host: pgo.test' \
      -H 'accept-encoding: identity' \
      "https://127.0.0.1:${HTTPS_PORT}/json/512"

    run_h2load "tls-fresh-h2-${index}" \
      -n 128 \
      -c 128 \
      -m 1 \
      --sni pgo.test \
      -H 'host: pgo.test' \
      -H 'accept-encoding: identity' \
      "https://127.0.0.1:${HTTPS_PORT}/json/512"
  done

  # AES-GCM and ChaCha20 TLS 1.3 cipher paths. Fail only if both are unavailable.
  aes_ok=0
  chacha_ok=0
  for _ in $(seq 1 128); do
    if printf 'GET /json/512 HTTP/1.1\r\nHost: pgo.test\r\nConnection: close\r\n\r\n' \
      | timeout 4 openssl s_client \
          -connect "127.0.0.1:${HTTPS_PORT}" \
          -servername pgo.test \
          -tls1_3 \
          -alpn http/1.1 \
          -ciphersuites TLS_AES_128_GCM_SHA256 \
          -quiet \
          >/dev/null 2>&1; then
      aes_ok=$((aes_ok + 1))
    fi

    if printf 'GET /json/512 HTTP/1.1\r\nHost: pgo.test\r\nConnection: close\r\n\r\n' \
      | timeout 4 openssl s_client \
          -connect "127.0.0.1:${HTTPS_PORT}" \
          -servername pgo.test \
          -tls1_3 \
          -alpn http/1.1 \
          -ciphersuites TLS_CHACHA20_POLY1305_SHA256 \
          -quiet \
          >/dev/null 2>&1; then
      chacha_ok=$((chacha_ok + 1))
    fi
  done
  if ((aes_ok == 0 && chacha_ok == 0)); then
    echo "neither AES-GCM nor ChaCha20 TLS 1.3 training succeeded" >&2
    exit 1
  fi

  # Obtain one TLS 1.3 session, then rotate the single-use ticket on every reuse.
  set +e
  {
    printf 'GET /json/512 HTTP/1.1\r\nHost: pgo.test\r\nConnection: keep-alive\r\n\r\n'
    sleep 2
  } \
    | timeout 4 openssl s_client \
        -connect "127.0.0.1:${HTTPS_PORT}" \
        -servername pgo.test \
        -tls1_3 \
        -alpn http/1.1 \
        -ign_eof \
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
  resumed=0
  for _ in $(seq 1 256); do
    set +e
    {
      printf 'GET /json/512 HTTP/1.1\r\nHost: pgo.test\r\nConnection: keep-alive\r\n\r\n'
      sleep 0.05
    } \
      | timeout 4 openssl s_client \
          -connect "127.0.0.1:${HTTPS_PORT}" \
          -servername pgo.test \
          -tls1_3 \
          -alpn http/1.1 \
          -sess_in "${OUTPUT_DIR}/tls-session.pem" \
          -sess_out "${OUTPUT_DIR}/tls-session-next.pem" \
          >>"${OUTPUT_DIR}/tls-resumption.log" 2>&1
    resume_rc=$?
    set -e

    if [[ ${resume_rc} -ne 0 && ${resume_rc} -ne 124 ]]; then
      echo "TLS resumption failed: exit=${resume_rc}" >&2
      exit 1
    fi
    test -s "${OUTPUT_DIR}/tls-session-next.pem"
    mv "${OUTPUT_DIR}/tls-session-next.pem" "${OUTPUT_DIR}/tls-session.pem"
    resumed=$((resumed + 1))
  done
  if ((resumed != 256)); then
    echo "TLS resumption workload was incomplete: ${resumed}/256" >&2
    exit 1
  fi

  reused_count=$(grep -c '^Reused, TLSv1.3' "${OUTPUT_DIR}/tls-resumption.log" || true)
  if ((reused_count != 256)); then
    echo "TLS sessions were not actually resumed: ${reused_count}/256" >&2
    exit 1
  fi
}

train_tail() {
  # Cache cold misses.
  for index in $(seq 1 64); do
    curl \
      --noproxy '*' \
      --fail \
      --silent \
      --show-error \
      --output /dev/null \
      -H 'Host: static.test' \
      -H 'Accept-Encoding: identity' \
      "http://127.0.0.1:${HTTP_PORT}/cold-${index}.bin"
  done

  # Slow upstream responses are concurrent so training does not take minutes.
  pause_pids=()
  for _ in $(seq 1 24); do
    curl \
      --noproxy '*' \
      --fail \
      --silent \
      --show-error \
      --output /dev/null \
      -H 'Host: music.test' \
      -H 'Accept-Encoding: identity' \
      "http://127.0.0.1:${HTTP_PORT}/pause/1048576" &
    pause_pids+=("$!")
  done
  for pid in "${pause_pids[@]}"; do
    wait "${pid}"
  done

  # Expected failures, resets and error-response generation.
  "${CLIENT_BIN}" \
    --port "${HTTP_PORT}" \
    --threads 4 \
    --requests-per-thread 1000 \
    --path /missing \
    --expected-status 404 \
    --expected-length 10 \
    --body-validation any

  "${CLIENT_BIN}" \
    --port "${HTTP_PORT}" \
    --threads 4 \
    --requests-per-thread 1000 \
    --path /status/500 \
    --expected-status 500 \
    --expected-length 14 \
    --body-validation any

  for _ in $(seq 1 256); do
    curl \
      --noproxy '*' \
      --silent \
      --show-error \
      --max-time 2 \
      --output /dev/null \
      -H 'Host: pgo.test' \
      "http://127.0.0.1:${HTTP_PORT}/reset" \
      || true
  done

  # Range/206, long streams and large request headers.
  for _ in $(seq 1 600); do
    curl \
      --noproxy '*' \
      --http1.1 \
      --fail \
      --silent \
      --show-error \
      --output /dev/null \
      -H 'Host: pgo.test' \
      -H 'Range: bytes=1048576-1114111' \
      "http://127.0.0.1:${HTTP_PORT}/bytes/10485760"
  done

  stream_pids=()
  for _ in $(seq 1 8); do
    curl \
      --noproxy '*' \
      --fail \
      --silent \
      --show-error \
      --output /dev/null \
      -H 'Host: music.test' \
      -H 'Accept-Encoding: identity' \
      "http://127.0.0.1:${HTTP_PORT}/stream/10485760" &
    stream_pids+=("$!")
  done
  for pid in "${stream_pids[@]}"; do
    wait "${pid}"
  done

  large_cookie="session=$(printf 'a%.0s' {1..4096})"
  for _ in $(seq 1 256); do
    curl \
      --noproxy '*' \
      --http1.1 \
      --fail \
      --silent \
      --show-error \
      --output /dev/null \
      -H 'Host: pgo.test' \
      -H "Cookie: ${large_cookie}" \
      -H 'Authorization: Bearer tail-latency-training-token' \
      "http://127.0.0.1:${HTTP_PORT}/json/512"
  done

  # Connection and stream burst for scheduler/pool tail behavior.
  "${CLIENT_BIN}" \
    --port "${HTTP_PORT}" \
    --threads 64 \
    --requests-per-thread 250 \
    --path /json/512 \
    --expected-length 512 \
    --body-validation any

  run_h2load tail-h2-burst \
    -n 12000 \
    -c 64 \
    -m 32 \
    -w 15 \
    -W 18 \
    --sni pgo.test \
    -H 'host: pgo.test' \
    -H 'accept-encoding: identity' \
    "https://127.0.0.1:${HTTPS_PORT}/json/512"
}

rm -rf "${OUTPUT_DIR}/runtime"
install -d -m 0700 "${OUTPUT_DIR}" "${RUNTIME_DIR}" "${STATIC_DIR}"

# Ephemeral ECDSA certificate used only during PGO training. It mirrors the
# production Let's Encrypt key algorithm without ever copying the real key.
openssl genpkey \
  -algorithm EC \
  -pkeyopt "ec_paramgen_curve:${ECDSA_CURVE}" \
  -out "${RUNTIME_DIR}/key.pem" \
  >"${OUTPUT_DIR}/openssl-key.log" 2>&1

openssl req \
  -new \
  -x509 \
  -sha256 \
  -days 1 \
  -key "${RUNTIME_DIR}/key.pem" \
  -subj '/CN=pgo.test' \
  -addext 'subjectAltName=DNS:pgo.test,DNS:static.test,DNS:music.test' \
  -out "${RUNTIME_DIR}/cert.pem" \
  >"${OUTPUT_DIR}/openssl-cert.log" 2>&1

chmod 0600 "${RUNTIME_DIR}/key.pem" "${RUNTIME_DIR}/cert.pem"
openssl pkey \
  -in "${RUNTIME_DIR}/key.pem" \
  -check \
  -noout \
  >>"${OUTPUT_DIR}/openssl-key.log" 2>&1

dd if=/dev/zero of="${STATIC_DIR}/hot.bin" bs=4096 count=1 status=none
for index in $(seq 1 64); do
  dd \
    if=/dev/zero \
    of="${STATIC_DIR}/cold-${index}.bin" \
    bs=512 \
    count=1 \
    status=none
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

"${BACKEND_BIN}" \
  --port "${BACKEND_PORT}" \
  >"${OUTPUT_DIR}/backend.log" 2>&1 &
BACKEND_PID=$!
wait_for_tcp "${BACKEND_PORT}" "${BACKEND_PID}" backend

LLVM_PROFILE_FILE="${OUTPUT_DIR}/pingora-%p-%m.profraw" \
  "${PINGORA_BIN}" \
    --config "${OUTPUT_DIR}/pingora.yaml" \
    >"${OUTPUT_DIR}/pingora.log" 2>&1 &
PINGORA_PID=$!
if ! wait_for_tcp "${HTTP_PORT}" "${PINGORA_PID}" Pingora; then
  sed -n '1,200p' "${OUTPUT_DIR}/pingora.log" >&2
  exit 1
fi

case "${SCENARIO}" in
  h1) train_h1 ;;
  h2) train_h2 ;;
  tls) train_tls ;;
  tail) train_tail ;;
esac

upstream_connections=$(
  curl \
    --noproxy '*' \
    --fail \
    --silent \
    --show-error \
    "http://127.0.0.1:${BACKEND_PORT}/stats/connections"
)
if [[ ! "${upstream_connections}" =~ ^[0-9]+$ ]]; then
  echo "invalid backend connection count: ${upstream_connections}" >&2
  exit 1
fi

cat >"${OUTPUT_DIR}/workload.txt" <<EOF
scenario=${SCENARIO}
ecdsa_curve=${ECDSA_CURVE}
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
