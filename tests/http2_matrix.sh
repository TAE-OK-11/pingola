#!/usr/bin/env bash
# Preserve every raw response under /tmp/pingola-h2-matrix and continue after
# individual failures so one protocol cannot hide results from the others.
set -uo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
RUNTIME=${PINGOLA_H2_RUNTIME:-/tmp/pingola-h2-matrix}
RESULTS=${RUNTIME}/results.tsv
GATEWAY_PID=
BACKEND_PID=
FAILURES=0

cleanup() {
  if [[ -n "${GATEWAY_PID}" ]]; then
    kill "${GATEWAY_PID}" 2>/dev/null || true
    wait "${GATEWAY_PID}" 2>/dev/null || true
  fi
  if [[ -n "${BACKEND_PID}" ]]; then
    kill "${BACKEND_PID}" 2>/dev/null || true
    wait "${BACKEND_PID}" 2>/dev/null || true
  fi
}
trap cleanup EXIT INT TERM

record() {
  printf '%s\t%s\t%s\t%s\t%s\t%s\n' "$@" >>"${RESULTS}"
}

run_curl_case() {
  local protocol=$1
  local method=$2
  local path=$3
  local expectation=$4
  local slug=${protocol}-${method}-${path//\//_}
  local body=${RUNTIME}/${slug}.body
  local headers=${RUNTIME}/${slug}.headers
  local errors=${RUNTIME}/${slug}.stderr
  local http_version=--http1.1
  [[ "${protocol}" == "h2" ]] && http_version=--http2

  local rc=0
  local method_args=("--request=${method}")
  if [[ "${method}" == "HEAD" ]]; then
    method_args=(--head)
  fi
  curl --noproxy '*' -ksS "${http_version}" "${method_args[@]}" \
    --resolve matrix.test:18444:127.0.0.1 \
    --dump-header "${headers}" --output "${body}" \
    "https://matrix.test:18444${path}" 2>"${errors}" || rc=$?

  local sha=-
  [[ -f "${body}" ]] && sha=$(sha256sum "${body}" | awk '{print $1}')
  local size=0
  [[ -f "${body}" ]] && size=$(stat -c '%s' "${body}")
  # curl --head writes the response headers to its output file; that is not an
  # HTTP response body and is deliberately excluded from body validation.
  [[ "${method}" == "HEAD" ]] && size=0
  local status=pass
  if [[ "${expectation}" == "success" && "${rc}" -ne 0 ]]; then
    status=fail
  elif [[ "${expectation}" == "early-eof" && "${rc}" -eq 0 ]]; then
    status=fail
  fi
  if [[ "${status}" == "fail" ]]; then
    FAILURES=$((FAILURES + 1))
  fi
  record "${protocol}" "${method}" "${path}" "${status}" "curl_rc=${rc},bytes=${size}" "sha256=${sha}"
}

rm -rf "${RUNTIME}"
mkdir -p "${RUNTIME}"
printf 'protocol\tmethod\tpath\tstatus\tdetail\tdigest\n' >"${RESULTS}"

openssl req -x509 -newkey rsa:2048 -nodes -days 1 \
  -subj "/CN=matrix.test" -addext "subjectAltName=DNS:matrix.test" \
  -keyout "${RUNTIME}/key.pem" -out "${RUNTIME}/cert.pem" >/dev/null 2>&1

cat >"${RUNTIME}/pingola.yaml" <<EOF
server:
  http_listen: ["127.0.0.1:18081"]
  https_listen: ["127.0.0.1:18444"]
  certificate: ${RUNTIME}/cert.pem
  private_key: ${RUNTIME}/key.pem
  threads: 1
  upstream_keepalive_pool_size: 128
  max_retries: 1
  graceful_shutdown_timeout_seconds: 2
  static_cache_bytes: 1048576
trusted_proxies: ["127.0.0.0/8"]
upstreams:
  matrix:
    address: "127.0.0.1:19091"
hosts:
  matrix:
    domains: ["matrix.test"]
    # AdGuard UI has neither a rate limit nor the lower per-service active
    # request cap, keeping this protocol test independent of limiter policy.
    handler: adguard-dns
    upstream: matrix
EOF

python3 "${ROOT}/tests/h2_matrix_backend.py" >"${RUNTIME}/backend.stdout" \
  2>"${RUNTIME}/backend.stderr" &
BACKEND_PID=$!

if [[ ! -x "${ROOT}/target/debug/pingola" ]]; then
  cargo build --manifest-path "${ROOT}/Cargo.toml" || exit 1
fi
RUST_LOG=debug "${ROOT}/target/debug/pingola" --config "${RUNTIME}/pingola.yaml" \
  >"${RUNTIME}/gateway.stdout" 2>"${RUNTIME}/gateway.stderr" &
GATEWAY_PID=$!

ready=0
for _ in {1..100}; do
  if curl --noproxy '*' -fsS -H 'host: matrix.test' \
    http://127.0.0.1:18081/pingola-health -o /dev/null 2>/dev/null; then
    ready=1
    break
  fi
  if ! kill -0 "${GATEWAY_PID}" 2>/dev/null; then
    break
  fi
  sleep 0.1
done
if [[ "${ready}" -ne 1 ]]; then
  echo "gateway failed to become ready; see ${RUNTIME}/gateway.stderr" >&2
  exit 1
fi

for protocol in h1 h2; do
  run_curl_case "${protocol}" GET /fixed/64 success
  run_curl_case "${protocol}" GET /fixed/4096 success
  run_curl_case "${protocol}" GET /chunked/4096 success
  run_curl_case "${protocol}" GET /trailer/4096 success
  run_curl_case "${protocol}" GET /empty/204 success
  run_curl_case "${protocol}" HEAD /fixed/64 success
  run_curl_case "${protocol}" GET /close/64 success
  run_curl_case "${protocol}" GET /keepalive/64 success
  run_curl_case "${protocol}" GET /early-eof/64 early-eof
done

expected64=$(python3 -c \
  'import hashlib; print(hashlib.sha256(bytes((i*17+11)%256 for i in range(64))).hexdigest())')
expected4096=$(python3 -c \
  'import hashlib; print(hashlib.sha256(bytes((i*17+11)%256 for i in range(4096))).hexdigest())')
while IFS=$'\t' read -r protocol method path status detail digest; do
  [[ "${protocol}" == "protocol" || "${status}" != "pass" || "${method}" == "HEAD" ]] && continue
  expected=
  case "${path}" in
    /fixed/64|/close/64|/keepalive/64) expected=${expected64} ;;
    /fixed/4096|/chunked/4096|/trailer/4096) expected=${expected4096} ;;
    /empty/204) expected=$(printf '' | sha256sum | awk '{print $1}') ;;
  esac
  if [[ -n "${expected}" && "${digest#sha256=}" != "${expected}" ]]; then
    FAILURES=$((FAILURES + 1))
    printf 'SHA-256 mismatch: %s %s %s\n' "${protocol}" "${method}" "${path}" >&2
  fi
done <"${RESULTS}"

# One TLS 1.3 connection carrying repeated and concurrent HTTP/2 streams.
nghttp --no-verify-peer -v -H ':authority: matrix.test' \
  https://127.0.0.1:18444/fixed/64 \
  https://127.0.0.1:18444/fixed/4096 \
  https://127.0.0.1:18444/chunked/4096 \
  >"${RUNTIME}/nghttp.stdout" 2>"${RUNTIME}/nghttp.stderr"
nghttp_rc=$?
[[ "${nghttp_rc}" -eq 0 ]] || FAILURES=$((FAILURES + 1))
record h2 GET multi-stream "$([[ "${nghttp_rc}" -eq 0 ]] && echo pass || echo fail)" \
  "nghttp_rc=${nghttp_rc}" "raw=nghttp.stdout"

for concurrency in 1 8 32; do
  # Keep one HTTP/2 connection and vary concurrent streams. Multi-connection
  # behavior is a separate benchmark dimension and must not multiply this
  # value by h2load's connection count.
  h2load -n $((concurrency * 20)) -c 1 -m "${concurrency}" \
    -H 'host: matrix.test' "https://127.0.0.1:18444/fixed/64" \
    >"${RUNTIME}/h2load-c${concurrency}.stdout" \
    2>"${RUNTIME}/h2load-c${concurrency}.stderr"
  rc=$?
  failed=$(awk '/failed, errored, timeout/ {gsub(/[^0-9,]/, "", $0); print $0}' \
    "${RUNTIME}/h2load-c${concurrency}.stdout")
  status=pass
  if [[ "${rc}" -ne 0 ]] || grep -Eq '([1-9][0-9]* failed|[1-9][0-9]* errored|[1-9][0-9]* timeout)' \
    "${RUNTIME}/h2load-c${concurrency}.stdout"; then
    status=fail
    FAILURES=$((FAILURES + 1))
  fi
  record h2 GET /fixed/64 "${status}" "concurrency=${concurrency},rc=${rc},${failed}" \
    "raw=h2load-c${concurrency}.stdout"
done

openssl s_client -connect 127.0.0.1:18444 -servername matrix.test -tls1_3 -alpn h2 \
  </dev/null >"${RUNTIME}/openssl.stdout" 2>"${RUNTIME}/openssl.stderr"
openssl_rc=$?
if [[ "${openssl_rc}" -ne 0 ]] || ! grep -q 'ALPN protocol: h2' "${RUNTIME}/openssl.stdout"; then
  FAILURES=$((FAILURES + 1))
  record h2 TLS tls1.3-alpn fail "openssl_rc=${openssl_rc}" raw=openssl.stdout
else
  record h2 TLS tls1.3-alpn pass "openssl_rc=${openssl_rc}" raw=openssl.stdout
fi

cat "${RESULTS}"
if [[ "${FAILURES}" -ne 0 ]]; then
  echo "HTTP matrix found ${FAILURES} failure(s); raw logs: ${RUNTIME}" >&2
  exit 1
fi
echo "HTTP matrix passed; raw logs: ${RUNTIME}"
