#!/usr/bin/env bash
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
RUNTIME=/tmp/pingora-upstream-http3
ORIGIN_LOG=${RUNTIME}/origin.log
FRONT_LOG=${RUNTIME}/front.log
FALLBACK_LOG=${RUNTIME}/fallback.log
BACKEND_LOG=${RUNTIME}/backend.log
ORIGIN_PID=
FRONT_PID=
FALLBACK_PID=
BACKEND_PID=

cleanup() {
  for pid in "${FALLBACK_PID}" "${FRONT_PID}" "${ORIGIN_PID}" "${BACKEND_PID}"; do
    if [[ -n "${pid}" ]]; then
      kill "${pid}" 2>/dev/null || true
      wait "${pid}" 2>/dev/null || true
    fi
  done
}
trap cleanup EXIT INT TERM

rm -rf "${RUNTIME}"
install -d -m 0755 "${RUNTIME}"
openssl req -x509 -newkey rsa:2048 -nodes -days 1 \
  -subj "/CN=origin.test" \
  -addext "subjectAltName=DNS:origin.test,DNS:front.test,DNS:fallback.test" \
  -keyout "${RUNTIME}/key.pem" -out "${RUNTIME}/cert.pem" >/dev/null 2>&1
chmod 0600 "${RUNTIME}/key.pem"
chmod 0644 "${RUNTIME}/cert.pem"

cargo build --manifest-path "${ROOT}/Cargo.toml" --locked --bin pingora
python3 "${ROOT}/tests/backend.py" >"${BACKEND_LOG}" 2>&1 &
BACKEND_PID=$!

RUST_LOG=info "${ROOT}/target/debug/pingora" \
  --config "${ROOT}/tests/fixtures/upstream_http3_origin.yaml" >"${ORIGIN_LOG}" 2>&1 &
ORIGIN_PID=$!

for _ in {1..100}; do
  if curl --noproxy '*' -fsS http://127.0.0.1:28081/pingora-ready -o /dev/null 2>/dev/null; then
    break
  fi
  if ! kill -0 "${ORIGIN_PID}" 2>/dev/null; then
    cat "${ORIGIN_LOG}" >&2
    exit 1
  fi
  sleep 0.1
done
kill -0 "${ORIGIN_PID}"

RUST_LOG=info "${ROOT}/target/debug/pingora" \
  --config "${ROOT}/tests/fixtures/upstream_http3_front.yaml" >"${FRONT_LOG}" 2>&1 &
FRONT_PID=$!
RUST_LOG=info "${ROOT}/target/debug/pingora" \
  --config "${ROOT}/tests/fixtures/upstream_http3_fallback.yaml" >"${FALLBACK_LOG}" 2>&1 &
FALLBACK_PID=$!

for port in 38081 38082; do
  log=${FRONT_LOG}
  pid=${FRONT_PID}
  if [[ "${port}" == 38082 ]]; then
    log=${FALLBACK_LOG}
    pid=${FALLBACK_PID}
  fi
  for _ in {1..100}; do
    if curl --noproxy '*' -fsS "http://127.0.0.1:${port}/pingora-ready" -o /dev/null 2>/dev/null; then
      break
    fi
    if ! kill -0 "${pid}" 2>/dev/null; then
      cat "${log}" >&2
      exit 1
    fi
    sleep 0.1
  done
  kill -0 "${pid}"
done

# Strict upstream HTTP/3: a normal Pingora downstream request must traverse the
# loopback h2c bridge and then the persistent QUIC/H3 connection to the origin.
strict_get=$(curl --noproxy '*' -fsS -H 'host: front.test' \
  http://127.0.0.1:38081/headers)
jq -e '.method == "GET" and .path == "/headers"' <<<"${strict_get}" >/dev/null
jq -e '.headers.host == "front.test"' <<<"${strict_get}" >/dev/null

strict_post=$(curl --noproxy '*' -fsS -H 'host: front.test' \
  -H 'content-type: application/octet-stream' \
  --data-binary 'upstream-http3-body' \
  http://127.0.0.1:38081/post)
jq -e '.method == "POST" and .body_length > 0' <<<"${strict_post}" >/dev/null

strict_second=$(curl --noproxy '*' -fsS -H 'host: front.test' \
  http://127.0.0.1:38081/reuse)
jq -e '.method == "GET" and .path == "/reuse"' <<<"${strict_second}" >/dev/null

for _ in {1..100}; do
  if grep -q 'upstream HTTP/3 session ticket cached upstream=origin' "${FRONT_LOG}"; then
    break
  fi
  sleep 0.1
done
grep -q 'upstream HTTP/3 established upstream=origin peer=127.0.0.1:28443' "${FRONT_LOG}"
grep -q 'upstream HTTP/3 session ticket cached upstream=origin' "${FRONT_LOG}"
# All three warm requests above should share the first H3 connection.
[[ $(grep -c 'upstream HTTP/3 established upstream=origin peer=127.0.0.1:28443' "${FRONT_LOG}") -eq 1 ]]

# The origin and front use a 1-second QUIC idle timeout. After the warm
# connection expires, the bridge deliberately waits for the next replay-safe
# request before reconnecting so that the cached ticket and GET can be emitted
# together as true TLS 0-RTT.
sleep 2
zero_rtt_get=$(curl --noproxy '*' -fsS -H 'host: front.test' \
  http://127.0.0.1:38081/zero-rtt)
jq -e '.method == "GET" and .path == "/zero-rtt"' <<<"${zero_rtt_get}" >/dev/null

for _ in {1..100}; do
  if grep -q 'upstream HTTP/3 early-data request sent stream=' "${FRONT_LOG}" \
    && grep -q 'HTTP/3 early-data request accepted' "${ORIGIN_LOG}"; then
    break
  fi
  sleep 0.1
done
grep -q 'upstream HTTP/3 early-data request sent stream=' "${FRONT_LOG}"
grep -q 'HTTP/3 early-data request accepted' "${ORIGIN_LOG}"
grep -q 'upstream HTTP/3 established upstream=origin peer=127.0.0.1:28443 resumed=true' "${FRONT_LOG}"

early_before=$(grep -c 'upstream HTTP/3 early-data request sent stream=' "${FRONT_LOG}" || true)
sleep 2
safe_post=$(curl --noproxy '*' -fsS -H 'host: front.test' \
  --data-binary 'must-wait-for-1rtt' \
  http://127.0.0.1:38081/not-early)
jq -e '.method == "POST" and .body_length > 0' <<<"${safe_post}" >/dev/null
early_after=$(grep -c 'upstream HTTP/3 early-data request sent stream=' "${FRONT_LOG}" || true)
[[ "${early_after}" -eq "${early_before}" ]]

# Preferred mode points at a TCP/TLS listener where no UDP listener exists.
# The H3 path therefore remains unhealthy and requests must transparently use
# the existing Pingora H2/H1 TLS peer instead.
fallback_get=$(curl --noproxy '*' -fsS -H 'host: fallback.test' \
  http://127.0.0.1:38082/fallback)
jq -e '.method == "GET" and .path == "/fallback"' <<<"${fallback_get}" >/dev/null
if grep -q 'upstream HTTP/3 established upstream=origin peer=127.0.0.1:28453' "${FALLBACK_LOG}"; then
  echo 'HTTP/3 unexpectedly established on the TCP-only fallback port' >&2
  exit 1
fi

grep -q 'upstream HTTP/3 bridge started: upstream=origin' "${FRONT_LOG}"
grep -q 'upstream HTTP/3 bridge started: upstream=origin' "${FALLBACK_LOG}"

echo 'Upstream HTTP/3 strict mode, connection reuse, request-body streaming, replay-safe 0-RTT, and H3-preferred TCP fallback tests passed'
