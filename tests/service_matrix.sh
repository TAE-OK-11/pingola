#!/usr/bin/env bash
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
RUNTIME=/tmp/pingora-service-matrix
BIN=${ROOT}/target/debug/pingora
GATEWAY_PID=
BACKEND_PID=
MONITOR_PID=

cleanup() {
  for pid in "${MONITOR_PID}" "${GATEWAY_PID}" "${BACKEND_PID}"; do
    if [[ -n "${pid}" ]]; then
      kill "${pid}" 2>/dev/null || true
      wait "${pid}" 2>/dev/null || true
    fi
  done
}
trap cleanup EXIT INT TERM

proxy_sha() {
  local host=$1
  local path=$2
  curl --noproxy '*' -ksS --http2 --resolve "${host}:18550:127.0.0.1" \
    "https://${host}:18550${path}" | sha256sum | awk '{print $1}'
}

direct_sha() {
  curl --noproxy '*' -sS "http://127.0.0.1:19996$1" | \
    sha256sum | awk '{print $1}'
}

rm -rf "${RUNTIME}"
mkdir -p "${RUNTIME}"
openssl req -x509 -newkey rsa:2048 -nodes -days 1 \
  -subj '/CN=nav.test' \
  -addext 'subjectAltName=DNS:nav.test,DNS:vault.test,DNS:couch.test,DNS:dns.test' \
  -keyout "${RUNTIME}/key.pem" -out "${RUNTIME}/cert.pem" >/dev/null 2>&1

cat >"${RUNTIME}/pingora.yaml" <<EOF
server:
  http_listen: ["127.0.0.1:18549"]
  https_listen: ["127.0.0.1:18550"]
  certificate: ${RUNTIME}/cert.pem
  private_key: ${RUNTIME}/key.pem
  health_socket: ${RUNTIME}/health.sock
  threads: 1
  upstream_keepalive_pool_size: 128
  max_retries: 2
  graceful_shutdown_timeout_seconds: 2
  static_cache_bytes: 1048576
  http2_max_concurrent_streams: 128
trusted_proxies: ["127.0.0.0/8"]
upstreams:
  backend:
    address: "127.0.0.1:19996"
  adguard_dns_doh:
    address: "127.0.0.1:19996"
hosts:
  nav:
    domains: ["nav.test"]
    handler: navidrome-main
    upstream: backend
  vault:
    domains: ["vault.test"]
    handler: vaultwarden
    upstream: backend
    max_body_bytes: 20971520
  couch:
    domains: ["couch.test"]
    handler: couchdb
    upstream: backend
    max_body_bytes: 20971520
  dns:
    domains: ["dns.test"]
    handler: adguard-dns
    upstream: backend
    max_body_bytes: 65536
route_limits:
  navidrome_stream: { rate_per_second: 0, active_requests: 64 }
  navidrome_cover: { rate_per_second: 0, active_requests: 64 }
  navidrome_api: { rate_per_second: 0, active_requests: 64 }
  vaultwarden_auth: { rate_per_second: 0, active_requests: 32 }
  vaultwarden_hub: { rate_per_second: 0, active_requests: 32 }
  vaultwarden: { rate_per_second: 0, active_requests: 32 }
  couchdb: { rate_per_second: 0, active_requests: 32 }
  doh: { rate_per_second: 0, active_requests: 128 }
  adguard_ui: { rate_per_second: 0, active_requests: 32 }
EOF

DISCONNECT_MARKER="${RUNTIME}/client-disconnect" \
  python3 "${ROOT}/tests/service_backend.py" >"${RUNTIME}/backend.stdout" \
  2>"${RUNTIME}/backend.stderr" &
BACKEND_PID=$!
RUST_LOG=warn "${BIN}" --config "${RUNTIME}/pingora.yaml" \
  >"${RUNTIME}/gateway.stdout" 2>"${RUNTIME}/gateway.stderr" &
GATEWAY_PID=$!
for _ in {1..100}; do
  if "${BIN}" --config "${RUNTIME}/pingora.yaml" --healthcheck >/dev/null 2>&1; then
    break
  fi
  sleep 0.1
done
kill -0 "${GATEWAY_PID}"

# Text/API routes may negotiate upstream compression. Audio streams,
# Vaultwarden auth/WebSocket and binary DoH routes must not.
api_headers=$(curl --noproxy '*' -ksS --http2 --resolve nav.test:18550:127.0.0.1 \
  -H 'accept-encoding: gzip' https://nav.test:18550/headers)
stream_headers=$(curl --noproxy '*' -ksS --http2 --resolve nav.test:18550:127.0.0.1 \
  -H 'accept-encoding: gzip' https://nav.test:18550/stream/headers)
vault_headers=$(curl --noproxy '*' --compressed -ksS --http2 \
  --resolve vault.test:18550:127.0.0.1 -D "${RUNTIME}/vault-compression.headers" \
  -H 'accept-encoding: gzip' https://vault.test:18550/headers)
couch_headers=$(curl --noproxy '*' --compressed -ksS --http2 \
  --resolve couch.test:18550:127.0.0.1 -D "${RUNTIME}/couch-compression.headers" \
  -H 'accept-encoding: gzip' https://couch.test:18550/headers)
jq -e '.accept_encoding == "gzip"' <<<"${api_headers}" >/dev/null
jq -e '.accept_encoding == null' <<<"${stream_headers}" >/dev/null
jq -e '.accept_encoding == null' <<<"${vault_headers}" >/dev/null
jq -e '.accept_encoding == null' <<<"${couch_headers}" >/dev/null
grep -qi '^content-encoding: gzip' "${RUNTIME}/vault-compression.headers"
grep -qi '^content-encoding: gzip' "${RUNTIME}/couch-compression.headers"

curl --noproxy '*' -ksS --http2 --resolve vault.test:18550:127.0.0.1 \
  -H 'accept-encoding: gzip;q=0' -D "${RUNTIME}/vault-q0.headers" \
  https://vault.test:18550/headers -o "${RUNTIME}/vault-q0.body"
! grep -qi '^content-encoding:' "${RUNTIME}/vault-q0.headers"
jq -e '.padding | length == 2048' "${RUNTIME}/vault-q0.body" >/dev/null
status=$(curl --noproxy '*' -ksS --http2 -o /dev/null -w '%{http_code}' \
  --resolve vault.test:18550:127.0.0.1 \
  -H 'accept-encoding: identity;q=0, *;q=0' \
  https://vault.test:18550/headers)
[[ "${status}" == 406 ]]

curl --noproxy '*' -ksS --http2 --resolve couch.test:18550:127.0.0.1 \
  -H 'accept-encoding: gzip' -D "${RUNTIME}/couch-no-transform.headers" \
  https://couch.test:18550/headers-no-transform \
  -o "${RUNTIME}/couch-no-transform.body"
! grep -qi '^content-encoding:' "${RUNTIME}/couch-no-transform.headers"
jq -e '.padding | length == 2048' "${RUNTIME}/couch-no-transform.body" >/dev/null

curl --noproxy '*' -ksS --http2 --resolve vault.test:18550:127.0.0.1 \
  -H 'accept-encoding: gzip' -D "${RUNTIME}/vault-binary.headers" \
  https://vault.test:18550/attachment/1048576 \
  -o "${RUNTIME}/vault-binary.body"
! grep -qi '^content-encoding:' "${RUNTIME}/vault-binary.headers"
[[ $(sha256sum "${RUNTIME}/vault-binary.body" | awk '{print $1}') == \
  $(direct_sha /attachment/1048576) ]]

# If identity is explicitly forbidden, a response that cannot legally or
# safely be transformed must be rejected instead of silently sending identity.
expect_identity_rejected() {
  local host=$1
  local path=$2
  shift 2
  local status
  status=$(curl --noproxy '*' -ksS --http2 -o /dev/null -w '%{http_code}' \
    --resolve "${host}:18550:127.0.0.1" \
    -H 'accept-encoding: gzip, identity;q=0' "$@" \
    "https://${host}:18550${path}")
  [[ "${status}" == 406 ]]
}
expect_identity_rejected vault.test /attachment/100
expect_identity_rejected couch.test /headers-no-transform
expect_identity_rejected couch.test /replication/4096 -H 'range: bytes=0-2047'

for endpoint in empty/204 not-modified/304; do
  expected=${endpoint##*/}
  status=$(curl --noproxy '*' -ksS --http2 -o /dev/null -w '%{http_code}' \
    --resolve "vault.test:18550:127.0.0.1" \
    -H 'accept-encoding: gzip, identity;q=0' \
    "https://vault.test:18550/${endpoint}")
  [[ "${status}" == "${expected}" ]]
done
status=$(curl --noproxy '*' -ksS --http2 --head -o /dev/null -w '%{http_code}' \
  --resolve "vault.test:18550:127.0.0.1" \
  -H 'accept-encoding: gzip, identity;q=0' \
  https://vault.test:18550/stream/100)
[[ "${status}" == 406 ]]

for size in 1048576 10485760 104857600; do
  expected=$(direct_sha "/stream/${size}")
  actual=$(proxy_sha nav.test "/stream/${size}")
  [[ "${actual}" == "${expected}" ]]
done

for mode in chunked slow pause; do
  expected=$(direct_sha "/stream-${mode}/1048576")
  actual=$(proxy_sha nav.test "/stream-${mode}/1048576")
  [[ "${actual}" == "${expected}" ]]
done

# Range/seek headers and body are preserved.
curl --noproxy '*' -sS -H 'range: bytes=65536-131071' \
  http://127.0.0.1:19996/stream/1048576 -o "${RUNTIME}/range-direct"
curl --noproxy '*' -ksS --http2 --resolve nav.test:18550:127.0.0.1 \
  -H 'range: bytes=65536-131071' -D "${RUNTIME}/range.headers" \
  https://nav.test:18550/stream/1048576 -o "${RUNTIME}/range-proxy"
cmp "${RUNTIME}/range-direct" "${RUNTIME}/range-proxy"
grep -q '^HTTP/2 206' "${RUNTIME}/range.headers"
grep -qi '^content-range: bytes 65536-131071/1048576' "${RUNTIME}/range.headers"
grep -qi '^accept-ranges: bytes' "${RUNTIME}/range.headers"

curl --noproxy '*' -ksSI --http2 --resolve nav.test:18550:127.0.0.1 \
  https://nav.test:18550/stream/1048576 >"${RUNTIME}/head.headers"
grep -qi '^content-length: 1048576' "${RUNTIME}/head.headers"
status=$(curl --noproxy '*' -ksS --http2 -o /dev/null -w '%{http_code}' \
  --resolve nav.test:18550:127.0.0.1 -H 'if-none-match: "synthetic-1048576"' \
  https://nav.test:18550/stream/1048576)
[[ "${status}" == 304 ]]

# Vaultwarden attachment and CouchDB chunked replication remain byte-exact.
[[ $(proxy_sha vault.test /attachment/1048576) == $(direct_sha /attachment/1048576) ]]
[[ $(proxy_sha couch.test /replication/1048576) == $(direct_sha /replication/1048576) ]]

# DoH GET/POST stays binary, uncompressed and no-store over H2.
curl --noproxy '*' -ksS --http2 --resolve dns.test:18550:127.0.0.1 \
  -D "${RUNTIME}/doh-get.headers" https://dns.test:18550/dns-query \
  -o "${RUNTIME}/doh-get.body"
grep -qi '^cache-control: no-store' "${RUNTIME}/doh-get.headers"
printf '\x00\x01query' >"${RUNTIME}/doh-query"
curl --noproxy '*' -ksS --http2 --resolve dns.test:18550:127.0.0.1 \
  -H 'content-type: application/dns-message' --data-binary @"${RUNTIME}/doh-query" \
  https://dns.test:18550/dns-query -o "${RUNTIME}/doh-post.body"
cmp "${RUNTIME}/doh-query" "${RUNTIME}/doh-post.body"
h2load -n 200 -c 2 -m 32 -H 'host: dns.test' \
  https://127.0.0.1:18550/dns-query >"${RUNTIME}/doh-h2load.log" 2>&1
! grep -Eq '([1-9][0-9]* failed|[1-9][0-9]* errored|[1-9][0-9]* timeout)' \
  "${RUNTIME}/doh-h2load.log"

# Early upstream EOF must be surfaced, not silently accepted as a valid body.
reset_rc=0
curl --noproxy '*' -ksS --http2 --resolve nav.test:18550:127.0.0.1 \
  https://nav.test:18550/stream-reset/1048576 -o /dev/null \
  2>"${RUNTIME}/reset.stderr" || reset_rc=$?
[[ "${reset_rc}" -ne 0 ]]

# A slow client disconnect should reach the upstream promptly.
set +o pipefail
curl --noproxy '*' -ksS --http2 --resolve nav.test:18550:127.0.0.1 \
  https://nav.test:18550/stream-slow/104857600 2>"${RUNTIME}/disconnect.stderr" | \
  head -c 65536 >/dev/null
set -o pipefail
for _ in {1..100}; do
  [[ -f "${RUNTIME}/client-disconnect" ]] && break
  sleep 0.05
done
[[ -f "${RUNTIME}/client-disconnect" ]]

rss_kib=$(awk '/VmRSS:/ {print $2}' "/proc/${GATEWAY_PID}/status")
echo "service traffic matrix passed; gateway_rss_kib=${rss_kib}; logs=${RUNTIME}"
