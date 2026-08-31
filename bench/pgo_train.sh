#!/usr/bin/env bash
set -euo pipefail

# shellcheck source=bench/pgo_train_scale.sh
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/pgo_train_scale.sh"

PINGORA_BIN=${1:?usage: pgo_train.sh PINGORA_BIN BACKEND_BIN CLIENT_BIN OUTPUT_DIR SCENARIO}
BACKEND_BIN=${2:?usage: pgo_train.sh PINGORA_BIN BACKEND_BIN CLIENT_BIN OUTPUT_DIR SCENARIO}
CLIENT_BIN=${3:?usage: pgo_train.sh PINGORA_BIN BACKEND_BIN CLIENT_BIN OUTPUT_DIR SCENARIO}
OUTPUT_DIR=${4:?usage: pgo_train.sh PINGORA_BIN BACKEND_BIN CLIENT_BIN OUTPUT_DIR SCENARIO}
SCENARIO=${5:?usage: pgo_train.sh PINGORA_BIN BACKEND_BIN CLIENT_BIN OUTPUT_DIR SCENARIO}

ECDSA_CURVE=${PGO_ECDSA_CURVE:-prime256v1}
REQUIRE_PROFILE=${PGO_REQUIRE_PROFILE:-true}
HTTP_PORT=${PGO_HTTP_PORT:-80}
HTTPS_PORT=${PGO_HTTPS_PORT:-443}
BACKEND_PORT=${PGO_BACKEND_PORT:-19000}
ROUND=${PGO_TRAIN_ROUND:-1}
RUNTIME_DIR=${OUTPUT_DIR}/runtime
STATIC_DIR=${RUNTIME_DIR}/static
BACKEND_PID=
PINGORA_PID=

case "${SCENARIO}" in
  h1|h2|tls|tail|zstd|light-json|bulk|internal|compress-br) ;;
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

wait_tcp() {
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

train_h1() {
  "${CLIENT_BIN}" --port "${HTTP_PORT}" --threads 1 --requests-per-thread "$(pgo_train_scale 16000)"
  "${CLIENT_BIN}" --port "${HTTP_PORT}" --threads 8 --requests-per-thread "$(pgo_train_scale 10000)"
  "${CLIENT_BIN}" --port "${HTTP_PORT}" --threads 4 --requests-per-thread "$(pgo_train_scale 7000)" \
    --path /json/512 --expected-length 512 --body-validation any
  "${CLIENT_BIN}" --port "${HTTP_PORT}" --threads 2 --requests-per-thread "$(pgo_train_scale 1500)" \
    --path /json/65536 --expected-length 65536 --body-validation any

  for _ in $(seq 1 "$(pgo_train_scale 48)"); do
    "${CLIENT_BIN}" --port "${HTTP_PORT}" --threads 16 --requests-per-thread 16
  done

  run_h2load h1-tls --h1 -n "$(pgo_train_scale 48000)" -c 16 -m 1 --sni pgo.test \
    -H 'host: pgo.test' -H 'accept-encoding: identity' \
    "https://127.0.0.1:${HTTPS_PORT}/json/512"

  for _ in $(seq 1 "$(pgo_train_scale 800)"); do
    printf '\x00\x01\x00\x00\x00\x01\x00\x00\x00\x00\x00\x00' | \
      curl --noproxy '*' --http1.1 --silent --show-error --output /dev/null \
        -H 'Host: dns.test' -H 'Content-Type: application/dns-message' \
        --data-binary @- "http://127.0.0.1:${HTTP_PORT}/dns-query"
  done

  for _ in $(seq 1 "$(pgo_train_scale 1000)"); do
    curl --noproxy '*' --http1.1 --fail --silent --show-error --output /dev/null \
      -H 'Host: cdn.test' -H 'Range: bytes=4096-8191' \
      "http://127.0.0.1:${HTTP_PORT}/bytes/1048576"
  done

  for _ in $(seq 1 48); do
    curl --noproxy '*' --http1.1 --fail --silent --show-error --output /dev/null \
      -H 'Host: music.test' -H 'Accept-Encoding: identity' \
      "http://127.0.0.1:${HTTP_PORT}/stream/1048576"
  done

  for host in pgo.test couch.test; do
    for _ in $(seq 1 "$(pgo_train_scale 1000)"); do
      curl --noproxy '*' --http1.1 --compressed --fail --silent --show-error --output /dev/null \
        -H "Host: ${host}" -H 'Accept-Encoding: gzip' \
        "http://127.0.0.1:${HTTP_PORT}/json/65536"
    done
  done
}

train_h2() {
  run_h2load h2-low -n "$(pgo_train_scale 12000)" -c 1 -m 1 -w 16 -W 16 --sni pgo.test \
    -H 'host: pgo.test' -H 'accept-encoding: identity' \
    "https://127.0.0.1:${HTTPS_PORT}/json/512"

  run_h2load h2-normal -n "$(pgo_train_scale 48000)" -c 4 -m 16 -w 16 -W 20 --sni pgo.test \
    -H 'host: pgo.test' -H 'accept-encoding: identity' \
    -H 'authorization: Bearer pgo-training-token' \
    -H 'cookie: session=pgo; preferences=h2' \
    "https://127.0.0.1:${HTTPS_PORT}/json/512"

  run_h2load h2-high -n "$(pgo_train_scale 64000)" -c 16 -m 32 -w 16 -W 20 --sni pgo.test \
    -H 'host: pgo.test' -H 'accept-encoding: identity' \
    "https://127.0.0.1:${HTTPS_PORT}/bytes/4096"

  run_h2load h2-large-json -n "$(pgo_train_scale 8000)" -c 4 -m 8 -w 16 -W 20 --sni pgo.test \
    -H 'host: pgo.test' -H 'accept-encoding: identity' \
    "https://127.0.0.1:${HTTPS_PORT}/json/65536"

  run_h2load h2-stream -n "$(pgo_train_scale 512)" -c 4 -m 4 -w 16 -W 20 --sni music.test \
    -H 'host: music.test' -H 'accept-encoding: identity' \
    "https://127.0.0.1:${HTTPS_PORT}/stream/1048576"

  run_h2load h2-static -n "$(pgo_train_scale 24000)" -c 4 -m 32 -w 16 -W 20 --sni static.test \
    -H 'host: static.test' -H 'accept-encoding: identity' \
    "https://127.0.0.1:${HTTPS_PORT}/hot.bin"

  run_h2load h2-couch -n "$(pgo_train_scale 12000)" -c 4 -m 16 -w 16 -W 20 --sni couch.test \
    -H 'host: couch.test' -H 'accept-encoding: gzip' \
    "https://127.0.0.1:${HTTPS_PORT}/json/65536"

  run_h2load h2-doh -n "$(pgo_train_scale 16000)" -c 4 -m 32 -w 16 -W 20 --sni dns.test \
    -H 'host: dns.test' -H 'accept-encoding: identity' \
    "https://127.0.0.1:${HTTPS_PORT}/dns-query"

  for _ in $(seq 1 "$(pgo_train_scale 400)"); do
    printf '\x00\x01\x00\x00\x00\x01\x00\x00\x00\x00\x00\x00' | \
      curl --noproxy '*' --http2 --insecure --fail --silent --show-error --output /dev/null \
        --resolve "dns.test:${HTTPS_PORT}:127.0.0.1" \
        -H 'Host: dns.test' -H 'Content-Type: application/dns-message' \
        --data-binary @- "https://dns.test:${HTTPS_PORT}/dns-query"
  done

  run_h2load h2-cover -n "$(pgo_train_scale 2000)" -c 4 -m 8 -w 16 -W 20 --sni music.test \
    -H 'host: music.test' -H 'accept-encoding: identity' \
    "https://127.0.0.1:${HTTPS_PORT}/rest/getCoverArt/test/65536"

  for _ in $(seq 1 512); do
    curl --noproxy '*' --http2 --insecure --fail --silent --show-error --output /dev/null \
      --resolve "cdn.test:${HTTPS_PORT}:127.0.0.1" \
      -H 'Range: bytes=65536-131071' -H 'Accept-Encoding: identity' \
      "https://cdn.test:${HTTPS_PORT}/bytes/1048576"
  done
}

tls_cipher() {
  local suite=$1
  local count=0

  for _ in $(seq 1 192); do
    if printf 'GET /json/512 HTTP/1.1\r\nHost: pgo.test\r\nConnection: close\r\n\r\n' | \
      timeout 4 openssl s_client -connect "127.0.0.1:${HTTPS_PORT}" \
        -servername pgo.test -tls1_3 -alpn http/1.1 -ciphersuites "${suite}" \
        -quiet >/dev/null 2>&1; then
      count=$((count + 1))
    fi
  done

  if ((count == 0)); then
    echo "TLS cipher training failed: ${suite}" >&2
    exit 1
  fi
}

train_tls() {
  for index in $(seq 1 "$(pgo_train_scale 12)"); do
    run_h2load "tls-fresh-h1-${index}" --h1 -n 128 -c 128 -m 1 --sni pgo.test \
      -H 'host: pgo.test' -H 'accept-encoding: identity' \
      "https://127.0.0.1:${HTTPS_PORT}/json/512"
    run_h2load "tls-fresh-h2-${index}" -n 128 -c 128 -m 1 --sni pgo.test \
      -H 'host: pgo.test' -H 'accept-encoding: identity' \
      "https://127.0.0.1:${HTTPS_PORT}/json/512"
  done

  tls_cipher TLS_AES_256_GCM_SHA384
  tls_cipher TLS_AES_128_GCM_SHA256
  tls_cipher TLS_CHACHA20_POLY1305_SHA256

  set +e
  { printf 'GET /json/512 HTTP/1.1\r\nHost: pgo.test\r\nConnection: keep-alive\r\n\r\n'; sleep 2; } | \
    timeout 4 openssl s_client -connect "127.0.0.1:${HTTPS_PORT}" \
      -servername pgo.test -tls1_3 -alpn http/1.1 -ign_eof \
      -sess_out "${OUTPUT_DIR}/tls-session.pem" \
      >"${OUTPUT_DIR}/tls-session-new.log" 2>&1
  session_rc=$?
  set -e

  if [[ ${session_rc} -ne 0 && ${session_rc} -ne 124 ]] || \
    [[ ! -s "${OUTPUT_DIR}/tls-session.pem" ]]; then
    echo "failed to obtain TLS session: exit=${session_rc}" >&2
    exit 1
  fi

  : >"${OUTPUT_DIR}/tls-resumption.log"
  for _ in $(seq 1 "$(pgo_train_scale 256)"); do
    set +e
    rm -f "${OUTPUT_DIR}/tls-session-next.pem"
    { printf 'GET /json/512 HTTP/1.1\r\nHost: pgo.test\r\nConnection: keep-alive\r\n\r\n'; sleep 0.2; } | \
      timeout 4 openssl s_client -connect "127.0.0.1:${HTTPS_PORT}" \
        -servername pgo.test -tls1_3 -alpn http/1.1 \
        -sess_in "${OUTPUT_DIR}/tls-session.pem" \
        -sess_out "${OUTPUT_DIR}/tls-session-next.pem" \
        >>"${OUTPUT_DIR}/tls-resumption.log" 2>&1
    resume_rc=$?
    set -e

    # Give the replacement NewSessionTicket enough time to arrive. At 50 ms
    # the handshake succeeded but sess_out was intermittently absent; 200 ms
    # was verified for 20 consecutive rotations on the target Cloudflare BoringSSL host.
    if [[ ${resume_rc} -ne 0 && ${resume_rc} -ne 124 ]] || \
      [[ ! -s "${OUTPUT_DIR}/tls-session-next.pem" ]]; then
      echo "TLS resumption failed: exit=${resume_rc}" >&2
      exit 1
    fi
    mv "${OUTPUT_DIR}/tls-session-next.pem" "${OUTPUT_DIR}/tls-session.pem"
  done

  reused_count=$(grep -c '^Reused, TLSv1.3' "${OUTPUT_DIR}/tls-resumption.log" || true)
  expected_reuse="$(pgo_train_scale 256)"
  if ((reused_count != expected_reuse)); then
    echo "TLS sessions were not actually resumed: ${reused_count}/${expected_reuse}" >&2
    exit 1
  fi
}

train_tail() {
  for index in $(seq 1 64); do
    curl --noproxy '*' --fail --silent --show-error --output /dev/null \
      -H 'Host: static.test' -H 'Accept-Encoding: identity' \
      "http://127.0.0.1:${HTTP_PORT}/cold-${index}.bin"
  done

  pause_pids=()
  for _ in $(seq 1 24); do
    curl --noproxy '*' --fail --silent --show-error --output /dev/null \
      -H 'Host: music.test' -H 'Accept-Encoding: identity' \
      "http://127.0.0.1:${HTTP_PORT}/pause/1048576" &
    pause_pids+=("$!")
  done
  for pid in "${pause_pids[@]}"; do
    wait "${pid}"
  done

  "${CLIENT_BIN}" --port "${HTTP_PORT}" --threads 4 --requests-per-thread 1000 \
    --path /missing --expected-status 404 --expected-length 10 --body-validation any
  "${CLIENT_BIN}" --port "${HTTP_PORT}" --threads 4 --requests-per-thread 1000 \
    --path /status/500 --expected-status 500 --expected-length 14 --body-validation any

  for _ in $(seq 1 "$(pgo_train_scale 256)"); do
    curl --noproxy '*' --silent --show-error --max-time 2 --output /dev/null \
      -H 'Host: pgo.test' "http://127.0.0.1:${HTTP_PORT}/reset" || true
  done

  for _ in $(seq 1 600); do
    curl --noproxy '*' --http1.1 --fail --silent --show-error --output /dev/null \
      -H 'Host: cdn.test' -H 'Range: bytes=1048576-1114111' \
      "http://127.0.0.1:${HTTP_PORT}/bytes/10485760"
  done

  stream_pids=()
  for _ in $(seq 1 8); do
    curl --noproxy '*' --fail --silent --show-error --output /dev/null \
      -H 'Host: music.test' -H 'Accept-Encoding: identity' \
      "http://127.0.0.1:${HTTP_PORT}/stream/10485760" &
    stream_pids+=("$!")
  done
  for pid in "${stream_pids[@]}"; do
    wait "${pid}"
  done

  large_cookie="session=$(printf 'a%.0s' {1..4096})"
  for _ in $(seq 1 "$(pgo_train_scale 256)"); do
    curl --noproxy '*' --http1.1 --fail --silent --show-error --output /dev/null \
      -H 'Host: pgo.test' -H "Cookie: ${large_cookie}" \
      -H 'Authorization: Bearer tail-latency-training-token' \
      "http://127.0.0.1:${HTTP_PORT}/json/512"
  done

  "${CLIENT_BIN}" --port "${HTTP_PORT}" --threads 64 --requests-per-thread 250 \
    --path /json/512 --expected-length 512 --body-validation any

  # The previous 64x32 burst created 2,048 simultaneous streams against a
  # synthetic HTTP/1 backend capped at 512 active connections. That trained
  # accidental 5xx overloads and made the build nondeterministic. Keep strict
  # zero-failure validation, but train two complementary bursts capped at 320
  # simultaneous streams with more total requests than before.
  run_h2load tail-h2-connection-burst -n "$(pgo_train_scale 10000)" -c 20 -m 16 -w 15 -W 18 \
    --sni pgo.test -H 'host: pgo.test' -H 'accept-encoding: identity' \
    "https://127.0.0.1:${HTTPS_PORT}/json/512"
  run_h2load tail-h2-stream-burst -n "$(pgo_train_scale 10000)" -c 10 -m 32 -w 15 -W 18 \
    --sni pgo.test -H 'host: pgo.test' -H 'accept-encoding: identity' \
    "https://127.0.0.1:${HTTPS_PORT}/json/512"
}

train_zstd() {
  for host in pgo.test couch.test; do
    for _ in $(seq 1 "$(pgo_train_scale 120)"); do
      curl --noproxy '*' --http2 --insecure --compressed --fail --silent --show-error --output /dev/null \
        --resolve "${host}:${HTTPS_PORT}:127.0.0.1" \
        -H "Host: ${host}" -H 'Accept-Encoding: zstd' \
        "https://${host}:${HTTPS_PORT}/json/65536"
    done
  done
  run_h2load zstd-vaultwarden -n "$(pgo_train_scale 6000)" -c 4 -m 16 -w 16 -W 20 --sni pgo.test \
    -H 'host: pgo.test' -H 'accept-encoding: zstd' \
    "https://127.0.0.1:${HTTPS_PORT}/json/65536"
  run_h2load zstd-couch -n "$(pgo_train_scale 3000)" -c 4 -m 8 -w 16 -W 20 --sni couch.test \
    -H 'host: couch.test' -H 'accept-encoding: zstd' \
    "https://127.0.0.1:${HTTPS_PORT}/json/65536"
  for _ in $(seq 1 "$(pgo_train_scale 300)"); do
    curl --noproxy '*' --http2 --insecure --compressed --fail --silent --show-error --output /dev/null \
      --resolve "static.test:${HTTPS_PORT}:127.0.0.1" \
      -H 'Host: static.test' -H 'Accept-Encoding: zstd' \
      "https://static.test:${HTTPS_PORT}/train.json"
  done
}

train_light_json() {
  run_h2load light-json-burst -n "$(pgo_train_scale 32000)" -c 16 -m 32 -w 16 -W 20 --sni pgo.test \
    -H 'host: pgo.test' -H 'accept-encoding: identity' \
    "https://127.0.0.1:${HTTPS_PORT}/json/512"
  "${CLIENT_BIN}" --port "${HTTP_PORT}" --threads 16 --requests-per-thread "$(pgo_train_scale 6000)" \
    --path /json/512 --expected-length 512 --body-validation any
  for _ in $(seq 1 "$(pgo_train_scale 1500)"); do
    curl --noproxy '*' --http1.1 --fail --silent --show-error --output /dev/null \
      -H 'Host: pgo.test' "http://127.0.0.1:${HTTP_PORT}/json/512"
  done
}

train_bulk() {
  run_h2load bulk-bytes -n "$(pgo_train_scale 600)" -c 4 -m 8 -w 16 -W 20 --sni cdn.test \
    -H 'host: cdn.test' -H 'accept-encoding: identity' \
    "https://127.0.0.1:${HTTPS_PORT}/bytes/4194304"
  BULK_SERIAL_N="$(pgo_train_scale 48)"
  if [[ "${PGO_TRAIN_FAST:-off}" == on ]] && (( BULK_SERIAL_N > 12 )); then
    BULK_SERIAL_N=12
  fi
  run_h2load bulk-serial -n "${BULK_SERIAL_N}" -c 1 -m 1 -w 16 -W 20 --sni pgo.test \
    -H 'host: pgo.test' -H 'accept-encoding: identity' \
    "https://127.0.0.1:${HTTPS_PORT}/bytes/10485760"
  for _ in $(seq 1 "$(pgo_train_scale 400)"); do
    curl --noproxy '*' --http2 --insecure --fail --silent --show-error --output /dev/null \
      --resolve "cdn.test:${HTTPS_PORT}:127.0.0.1" \
      -H 'Host: cdn.test' -H 'Range: bytes=1048576-2097151' -H 'Accept-Encoding: identity' \
      "https://cdn.test:${HTTPS_PORT}/bytes/10485760"
  done
  stream_pids=()
  for _ in $(seq 1 "$(pgo_train_scale 4)"); do
    curl --noproxy '*' --http2 --insecure --fail --silent --show-error --output /dev/null \
      --resolve "music.test:${HTTPS_PORT}:127.0.0.1" \
      -H 'Host: music.test' -H 'Accept-Encoding: identity' \
      "https://music.test:${HTTPS_PORT}/stream/4194304" &
    stream_pids+=("$!")
  done
  for pid in "${stream_pids[@]}"; do
    wait "${pid}"
  done

  run_h2load bulk-5m -n "$(pgo_train_scale 600)" -c 4 -m 8 -w 16 -W 20 --sni cdn.test \
    -H 'host: cdn.test' -H 'accept-encoding: identity' \
    "https://127.0.0.1:${HTTPS_PORT}/bytes/5242880"
  BULK_30M_N="$(pgo_train_scale 48)"
  if [[ "${PGO_TRAIN_FAST:-off}" == on ]] && (( BULK_30M_N > 12 )); then
    BULK_30M_N=12
  fi
  run_h2load bulk-30m -n "${BULK_30M_N}" -c 1 -m 1 -w 16 -W 20 --sni pgo.test \
    -H 'host: pgo.test' -H 'accept-encoding: identity' \
    "https://127.0.0.1:${HTTPS_PORT}/bytes/31457280"
  for _ in $(seq 1 "$(pgo_train_scale 400)"); do
    curl --noproxy '*' --http2 --insecure --fail --silent --show-error --output /dev/null \
      --resolve "cdn.test:${HTTPS_PORT}:127.0.0.1" \
      -H 'Host: cdn.test' -H 'Range: bytes=5242880-10485759' -H 'Accept-Encoding: identity' \
      "https://cdn.test:${HTTPS_PORT}/bytes/31457280"
  done
  stream_pids=()
  for _ in $(seq 1 "$(pgo_train_scale 4)"); do
    curl --noproxy '*' --http2 --insecure --fail --silent --show-error --output /dev/null \
      --resolve "music.test:${HTTPS_PORT}:127.0.0.1" \
      -H 'Host: music.test' -H 'Accept-Encoding: identity' \
      "https://music.test:${HTTPS_PORT}/stream/31457280" &
    stream_pids+=("$!")
  done
  for pid in "${stream_pids[@]}"; do
    wait "${pid}"
  done
}

train_internal() {
  run_h2load internal-static -n "$(pgo_train_scale 20000)" -c 8 -m 32 -w 16 -W 20 --sni static.test \
    -H 'host: static.test' -H 'accept-encoding: identity' \
    "https://127.0.0.1:${HTTPS_PORT}/hot.bin"
  static_etag=$(curl --noproxy '*' --silent --show-error -I \
    -H 'Host: static.test' "http://127.0.0.1:${HTTP_PORT}/hot.bin" \
    | awk -F': ' 'tolower($1)=="etag" {gsub(/\r/,"",$2); print $2; exit}')
  if [[ -z "${static_etag}" ]]; then
    echo "failed to capture static hot.bin ETag for 304 training" >&2
    exit 1
  fi
  for _ in $(seq 1 "$(pgo_train_scale 500)"); do
    curl --noproxy '*' --fail --silent --show-error --output /dev/null \
      -H 'Host: static.test' -H "If-None-Match: ${static_etag}" \
      "http://127.0.0.1:${HTTP_PORT}/hot.bin"
  done
  for _ in $(seq 1 "$(pgo_train_scale 250)"); do
    curl --noproxy '*' --http2 --insecure --fail --silent --show-error --output /dev/null \
      --resolve "static.test:${HTTPS_PORT}:127.0.0.1" \
      -H 'Host: static.test' -H "If-None-Match: ${static_etag}" \
      "https://static.test:${HTTPS_PORT}/hot.bin"
  done
  "${CLIENT_BIN}" --port "${HTTP_PORT}" --threads 32 --requests-per-thread "$(pgo_train_scale 400)" \
    --path /json/512 --expected-length 512 --body-validation any
  run_h2load internal-json-h2 -n "$(pgo_train_scale 12000)" -c 8 -m 16 -w 16 -W 20 --sni pgo.test \
    -H 'host: pgo.test' -H 'accept-encoding: identity' \
    "https://127.0.0.1:${HTTPS_PORT}/json/512"
  for _ in $(seq 1 "$(pgo_train_scale 64)"); do
    curl --noproxy '*' --http2 --insecure --fail --silent --show-error --output /dev/null \
      --resolve "pgo.test:${HTTPS_PORT}:127.0.0.1" \
      -H 'Host: pgo.test' -H 'Content-Type: application/json' \
      --data '{"email":"pgo@example.test"}' \
      "https://pgo.test:${HTTPS_PORT}/api/accounts/prelogin"
  done
}

train_compress_br() {
  for host in pgo.test couch.test; do
    for _ in $(seq 1 "$(pgo_train_scale 120)"); do
      curl --noproxy '*' --http2 --insecure --compressed --fail --silent --show-error --output /dev/null \
        --resolve "${host}:${HTTPS_PORT}:127.0.0.1" \
        -H "Host: ${host}" -H 'Accept-Encoding: br' \
        "https://${host}:${HTTPS_PORT}/json/65536"
    done
  done
  run_h2load br-vaultwarden -n "$(pgo_train_scale 6000)" -c 4 -m 16 -w 16 -W 20 --sni pgo.test \
    -H 'host: pgo.test' -H 'accept-encoding: br' \
    "https://127.0.0.1:${HTTPS_PORT}/json/65536"
  run_h2load br-couch -n "$(pgo_train_scale 3000)" -c 4 -m 8 -w 16 -W 20 --sni couch.test \
    -H 'host: couch.test' -H 'accept-encoding: br' \
    "https://127.0.0.1:${HTTPS_PORT}/json/65536"
  for _ in $(seq 1 "$(pgo_train_scale 300)"); do
    curl --noproxy '*' --http2 --insecure --compressed --fail --silent --show-error --output /dev/null \
      --resolve "static.test:${HTTPS_PORT}:127.0.0.1" \
      -H 'Host: static.test' -H 'Accept-Encoding: br' \
      "https://static.test:${HTTPS_PORT}/train.json"
  done
  for _ in $(seq 1 "$(pgo_train_scale 48)"); do
    curl --noproxy '*' --http2 --insecure --compressed --fail --silent --show-error --output /dev/null \
      --resolve "pgo.test:${HTTPS_PORT}:127.0.0.1" \
      -H 'Host: pgo.test' -H 'Accept-Encoding: br, zstd, gzip' \
      "https://pgo.test:${HTTPS_PORT}/json/65536"
  done
}

rm -rf "${RUNTIME_DIR}"
install -d -m 0700 "${OUTPUT_DIR}" "${RUNTIME_DIR}" "${STATIC_DIR}"

openssl genpkey -algorithm EC -pkeyopt "ec_paramgen_curve:${ECDSA_CURVE}" \
  -out "${RUNTIME_DIR}/key.pem" >"${OUTPUT_DIR}/openssl-key.log" 2>&1
openssl req -new -x509 -sha256 -days 1 -key "${RUNTIME_DIR}/key.pem" \
  -subj '/CN=pgo.test' \
  -addext 'subjectAltName=DNS:pgo.test,DNS:static.test,DNS:music.test,DNS:couch.test,DNS:dns.test,DNS:cdn.test' \
  -out "${RUNTIME_DIR}/cert.pem" >"${OUTPUT_DIR}/openssl-cert.log" 2>&1
chmod 0600 "${RUNTIME_DIR}/key.pem" "${RUNTIME_DIR}/cert.pem"
openssl pkey -in "${RUNTIME_DIR}/key.pem" -check -noout \
  >>"${OUTPUT_DIR}/openssl-key.log" 2>&1

dd if=/dev/zero of="${STATIC_DIR}/hot.bin" bs=4096 count=1 status=none
printf '%8192s' | tr ' ' 'x' >"${STATIC_DIR}/train.json"
for index in $(seq 1 64); do
  dd if=/dev/zero of="${STATIC_DIR}/cold-${index}.bin" bs=512 count=1 status=none
done

cat >"${OUTPUT_DIR}/pingora.yaml" <<EOF_YAML
server:
  http_listen: ["127.0.0.1:${HTTP_PORT}"]
  https_listen: ["127.0.0.1:${HTTPS_PORT}"]
  certificate: ${RUNTIME_DIR}/cert.pem
  private_key: ${RUNTIME_DIR}/key.pem
  health_socket: ${RUNTIME_DIR}/health.sock
  threads: 1
  upstream_keepalive_pool_size: 128
  downstream_keepalive_requests: 500
  max_retries: 2
  access_log: false
  http2_max_concurrent_streams: 32
  static_cache_bytes: 1048576
  # h2load multiplexes this benchmark through one loopback client identity.
  static_active_requests_per_client: 1000000
  graceful_shutdown_timeout_seconds: 2
trusted_proxies: ["127.0.0.0/8"]
upstreams:
  backend:
    address: "127.0.0.1:${BACKEND_PORT}"
    protocol: http1
    connect_timeout_seconds: 2
    read_timeout_seconds: 60
    write_timeout_seconds: 60
    idle_timeout_seconds: 30
  adguard_dns_doh:
    address: "127.0.0.1:${BACKEND_PORT}"
    protocol: http1
    connect_timeout_seconds: 2
    read_timeout_seconds: 60
    write_timeout_seconds: 60
    idle_timeout_seconds: 30
hosts:
  api: { domains: ["pgo.test"], handler: vaultwarden, upstream: backend }
  music: { domains: ["music.test"], handler: navidrome-main, upstream: backend }
  static: { domains: ["static.test"], handler: static, static_root: ${STATIC_DIR} }
  couch: { domains: ["couch.test"], handler: couchdb, upstream: backend }
  dns: { domains: ["dns.test"], handler: adguard-dns, upstream: backend, max_body_bytes: 65536 }
  cdn: { domains: ["cdn.test"], handler: navidrome-cdn, upstream: backend }
route_limits:
  vaultwarden: { rate_per_second: 0, active_requests: 0 }
  navidrome_api: { rate_per_second: 0, active_requests: 0 }
  navidrome_stream: { rate_per_second: 0, active_requests: 0 }
  navidrome_cover: { rate_per_second: 0, active_requests: 0 }
  couchdb: { rate_per_second: 0, active_requests: 0 }
  doh: { rate_per_second: 0, active_requests: 0 }
EOF_YAML

"${BACKEND_BIN}" --port "${BACKEND_PORT}" >"${OUTPUT_DIR}/backend.log" 2>&1 &
BACKEND_PID=$!
wait_tcp "${BACKEND_PORT}" "${BACKEND_PID}" backend

CHECK_PATTERN="${RUNTIME_DIR}/check-%p-%m.profraw"
if ! LLVM_PROFILE_FILE="${CHECK_PATTERN}" \
  "${PINGORA_BIN}" --config "${OUTPUT_DIR}/pingora.yaml" --check \
  >"${OUTPUT_DIR}/pingora-check.log" 2>&1; then
  echo "PGO configuration preflight failed: scenario=${SCENARIO} round=${ROUND}" >&2
  sed -n '1,240p' "${OUTPUT_DIR}/pingora-check.log" >&2
  exit 1
fi
rm -f "${RUNTIME_DIR}"/check-*.profraw

LLVM_PROFILE_FILE="${OUTPUT_DIR}/pingora-r${ROUND}-%p-%m.profraw" \
  "${PINGORA_BIN}" --config "${OUTPUT_DIR}/pingora.yaml" \
  >"${OUTPUT_DIR}/pingora.log" 2>&1 &
PINGORA_PID=$!
if ! wait_tcp "${HTTP_PORT}" "${PINGORA_PID}" Pingora; then
  sed -n '1,200p' "${OUTPUT_DIR}/pingora.log" >&2
  exit 1
fi

"train_${SCENARIO}"

upstream_connections=$(curl --noproxy '*' --fail --silent --show-error \
  "http://127.0.0.1:${BACKEND_PORT}/stats/connections")
if [[ ! "${upstream_connections}" =~ ^[0-9]+$ ]]; then
  echo "invalid backend connection count: ${upstream_connections}" >&2
  exit 1
fi

cat >"${OUTPUT_DIR}/workload.txt" <<EOF_WORKLOAD
scenario=${SCENARIO}
round=${ROUND}
ecdsa_curve=${ECDSA_CURVE}
http_port=${HTTP_PORT}
https_port=${HTTPS_PORT}
backend_port=${BACKEND_PORT}
backend_connections=${upstream_connections}
EOF_WORKLOAD

kill -TERM "${PINGORA_PID}"
wait "${PINGORA_PID}"
PINGORA_PID=

if [[ "${REQUIRE_PROFILE}" == true ]]; then
  compgen -G "${OUTPUT_DIR}/*.profraw" >/dev/null
elif [[ "${REQUIRE_PROFILE}" != false ]]; then
  echo 'PGO_REQUIRE_PROFILE must be true or false' >&2
  exit 2
fi
