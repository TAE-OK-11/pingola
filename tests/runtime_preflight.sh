#!/usr/bin/env bash
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
RUNTIME=/tmp/pingora-preflight
BIN=${ROOT}/target/debug/pingora
FAILURES=0

expect_failure() {
  local name=$1
  local pattern=$2
  shift 2
  local log=${RUNTIME}/${name}.log
  if "$@" >"${log}" 2>&1; then
    echo "${name}: unexpectedly succeeded" >&2
    FAILURES=$((FAILURES + 1))
    return
  fi
  if ! grep -Eq "${pattern}" "${log}"; then
    echo "${name}: expected pattern not found: ${pattern}" >&2
    cat "${log}" >&2
    FAILURES=$((FAILURES + 1))
  fi
}

write_config() {
  local output=$1
  local certificate=$2
  local private_key=$3
  local port=$4
  cat >"${output}" <<EOF
server:
  http_listen: []
  https_listen: ["127.0.0.1:${port}"]
  certificate: ${certificate}
  private_key: ${private_key}
  health_socket: ${RUNTIME}/health-${port}.sock
trusted_proxies: ["127.0.0.0/8"]
upstreams: {}
hosts:
  static:
    domains: ["static.test"]
    handler: static
    static_root: ${RUNTIME}/www
EOF
  chmod 0644 "${output}"
}

rm -rf "${RUNTIME}"
mkdir -p "${RUNTIME}/www" "${RUNTIME}/archive" "${RUNTIME}/live"
chmod 0755 "${RUNTIME}" "${RUNTIME}/www" "${RUNTIME}/archive" "${RUNTIME}/live"
printf 'preflight fixture\n' >"${RUNTIME}/www/index.html"

for identity in a b; do
  openssl req -x509 -newkey rsa:2048 -nodes -days 1 \
    -subj "/CN=${identity}.test" -addext "subjectAltName=DNS:${identity}.test" \
    -keyout "${RUNTIME}/${identity}.key" -out "${RUNTIME}/${identity}.crt" \
    >/dev/null 2>&1
done

write_config "${RUNTIME}/valid.yaml" "${RUNTIME}/a.crt" "${RUNTIME}/a.key" 443
"${BIN}" --config "${RUNTIME}/valid.yaml" --check >"${RUNTIME}/valid.log" 2>&1
grep -q '\[ok\] certificate/private key match' "${RUNTIME}/valid.log"

printf '%s\n' 'not a certificate' >"${RUNTIME}/invalid.crt"
write_config "${RUNTIME}/invalid.yaml" "${RUNTIME}/invalid.crt" "${RUNTIME}/a.key" 443
expect_failure invalid_pem 'certificate PEM parse.*contains no certificates' \
  "${BIN}" --config "${RUNTIME}/invalid.yaml" --check

write_config "${RUNTIME}/mismatch.yaml" "${RUNTIME}/a.crt" "${RUNTIME}/b.key" 443
expect_failure key_mismatch 'certificate/private key match.*rejected' \
  "${BIN}" --config "${RUNTIME}/mismatch.yaml" --check

ln -s ../archive/privkey7.pem "${RUNTIME}/live/privkey.pem"
write_config "${RUNTIME}/broken.yaml" "${RUNTIME}/a.crt" \
  "${RUNTIME}/live/privkey.pem" 443
expect_failure broken_symlink \
  'private key open.*symlink=true.*final_target=.*/archive/privkey7.pem.*target_exists=false' \
  "${BIN}" --config "${RUNTIME}/broken.yaml" --check

cat >"${RUNTIME}/invalid-upstream.yaml" <<EOF
server:
  http_listen: ["127.0.0.1:80"]
  https_listen: []
  health_socket: ${RUNTIME}/health-80.sock
trusted_proxies: ["127.0.0.0/8"]
upstreams:
  broken:
    address: "127.0.0.1:not-a-port"
hosts:
  app:
    domains: ["app.test"]
    handler: vaultwarden
    upstream: broken
EOF
expect_failure invalid_upstream \
  'upstream address broken.*configured=127.0.0.1:not-a-port.*resolution failed' \
  "${BIN}" --config "${RUNTIME}/invalid-upstream.yaml" --check

cp "${RUNTIME}/a.key" "${RUNTIME}/root-only.key"
chown 0:0 "${RUNTIME}/root-only.key"
chmod 0600 "${RUNTIME}/root-only.key"
chmod 0644 "${RUNTIME}/a.crt"
write_config "${RUNTIME}/unreadable.yaml" "${RUNTIME}/a.crt" \
  "${RUNTIME}/root-only.key" 443
cp "${BIN}" "${RUNTIME}/pingora"
chmod 0755 "${RUNTIME}/pingora"
expect_failure unreadable_key \
  'private key open.*process_uid=10001 process_gid=10001 owner_uid=0 owner_gid=0 mode=0600' \
  setpriv --reuid 10001 --regid 10001 --clear-groups \
  "${RUNTIME}/pingora" --config "${RUNTIME}/unreadable.yaml" --check
if grep -q -- '-----BEGIN.*PRIVATE KEY-----' "${RUNTIME}/unreadable_key.log"; then
  echo "private key material leaked to diagnostics" >&2
  FAILURES=$((FAILURES + 1))
fi

# --check deliberately does not bind, while --check-bind reports the exact
# occupied address and continues printing all other check results.
python3 -c 'import socket,time; s=socket.socket(); s.bind(("127.0.0.1",443)); s.listen(); time.sleep(30)' &
OCCUPIER_PID=$!
trap 'kill "${OCCUPIER_PID}" 2>/dev/null || true' EXIT
sleep 0.1
write_config "${RUNTIME}/occupied.yaml" "${RUNTIME}/a.crt" "${RUNTIME}/a.key" 443
"${BIN}" --config "${RUNTIME}/occupied.yaml" --check >"${RUNTIME}/occupied-no-bind.log" 2>&1
expect_failure occupied_bind 'listener bind HTTPS 127.0.0.1:443.*conflicting address 127.0.0.1:443' \
  "${BIN}" --config "${RUNTIME}/occupied.yaml" --check-bind
kill "${OCCUPIER_PID}" 2>/dev/null || true
wait "${OCCUPIER_PID}" 2>/dev/null || true
trap - EXIT

if [[ "${FAILURES}" -ne 0 ]]; then
  echo "runtime preflight tests failed: ${FAILURES}" >&2
  exit 1
fi
echo "runtime preflight tests passed"
