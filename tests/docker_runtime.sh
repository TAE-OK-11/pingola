#!/usr/bin/env bash
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
IMAGE=${PINGORA_TEST_IMAGE:-ghcr.io/tae-ok-11/pingora:local}
EXPECTED_ALLOCATOR=${PINGORA_EXPECTED_ALLOCATOR:-jemalloc}
EXPECTED_TARGET_CPU=${PINGORA_EXPECTED_TARGET_CPU:-x86-64-v2}
EXPECTED_LTO=${PINGORA_EXPECTED_LTO:-fat}
EXPECTED_TLS_PROVIDER=${PINGORA_EXPECTED_TLS_PROVIDER:-boringssl}
EXPECTED_PGO=${PINGORA_EXPECTED_PGO:-off}
RUNTIME=${PINGORA_DOCKER_TEST_RUNTIME:-/tmp/pingora-docker-runtime}
CONTAINERS=()

cleanup() {
  if ((${#CONTAINERS[@]})); then
    docker rm -f "${CONTAINERS[@]}" >/dev/null 2>&1 || true
  fi
  rm -rf "${RUNTIME}"
}
trap cleanup EXIT INT TERM

rm -rf "${RUNTIME}"
install -d -m 0755 "${RUNTIME}/cert"

write_config() {
  local file=$1
  local http=$2
  local https=$3
  local tls=${4:-false}
  local http3=${5:-'[]'}
  {
    printf 'server:\n'
    printf '  http_listen: %s\n' "${http}"
    printf '  https_listen: %s\n' "${https}"
    printf '  http3_listen: %s\n' "${http3}"
    if [[ "${http3}" != '[]' ]]; then
      printf '  http3_max_idle_timeout_seconds: 15\n'
      printf '  http3_max_concurrent_streams: 16\n'
    fi
    if [[ "${tls}" == true ]]; then
      printf '  certificate: /etc/pingora/cert/cert.pem\n'
      printf '  private_key: /etc/pingora/cert/key.pem\n'
    fi
    printf '  health_socket: /tmp/pingora/health.sock\n'
    printf '  graceful_shutdown_timeout_seconds: 1\n'
    printf 'trusted_proxies: ["127.0.0.0/8", "::1/128"]\n'
    printf 'upstreams:\n'
    printf '  unavailable:\n'
    printf '    address: "127.0.0.1:9"\n'
    printf 'hosts:\n'
    printf '  app:\n'
    printf '    domains: ["app.test"]\n'
    printf '    handler: vaultwarden\n'
    printf '    upstream: unavailable\n'
  } >"${file}"
  chmod 0644 "${file}"
}

start_container() {
  local name=$1
  local config=$2
  shift 2

  CONTAINERS+=("${name}")
  docker run --detach --name "${name}" --network host \
    --read-only --cap-drop ALL --cap-add NET_BIND_SERVICE \
    --security-opt no-new-privileges \
    --health-start-period 0s --health-interval 1s --health-timeout 2s --health-retries 10 \
    --tmpfs /tmp/pingora:rw,noexec,nosuid,nodev,uid=10001,gid=10001,mode=0770 \
    --volume "${config}:/etc/pingora/pingora.yaml:ro" \
    "$@" "${IMAGE}" >/dev/null

  for _ in {1..50}; do
    case $(docker inspect --format '{{if .State.Health}}{{.State.Health.Status}}{{else}}missing{{end}}' "${name}") in
      healthy) return ;;
      unhealthy|missing)
        docker inspect "${name}" >&2
        docker logs "${name}" >&2
        return 1
        ;;
    esac
    sleep 0.2
  done
  docker inspect --format '{{json .State.Health}}' "${name}" >&2
  docker logs "${name}" >&2
  return 1
}

assert_container_hardening() {
  local name=$1 native_pgo
  [[ $(docker inspect --format '{{.Config.User}}' "${name}") == 10001:10001 ]]
  [[ $(docker inspect --format '{{.HostConfig.ReadonlyRootfs}}' "${name}") == true ]]
  [[ $(docker inspect --format '{{json .HostConfig.CapDrop}}' "${name}") == '["ALL"]' ]]
  [[ $(docker inspect --format '{{json .HostConfig.CapAdd}}' "${name}") == '["CAP_NET_BIND_SERVICE"]' ]]
  docker exec "${name}" /usr/local/bin/pingora --allocator-info \
    | grep -q "^allocator=${EXPECTED_ALLOCATOR} "
  [[ $(docker inspect --format '{{index .Config.Labels "org.opencontainers.image.allocator"}}' "${name}") == "${EXPECTED_ALLOCATOR}" ]]
  [[ $(docker inspect --format '{{index .Config.Labels "org.opencontainers.image.rust.target-cpu"}}' "${name}") == "${EXPECTED_TARGET_CPU}" ]]
  [[ $(docker inspect --format '{{index .Config.Labels "org.opencontainers.image.rust.lto"}}' "${name}") == "${EXPECTED_LTO}" ]]
  [[ $(docker inspect --format '{{index .Config.Labels "org.opencontainers.image.tls.provider"}}' "${name}") == "${EXPECTED_TLS_PROVIDER}" ]]
  [[ $(docker inspect --format '{{index .Config.Labels "org.opencontainers.image.http3.provider"}}' "${name}") == quiche ]]
  [[ $(docker inspect --format '{{index .Config.Labels "org.opencontainers.image.http3.internal-protocol"}}' "${name}") == direct-gateway ]]
  [[ $(docker inspect --format '{{index .Config.Labels "org.opencontainers.image.quic.tls.provider"}}' "${name}") == boringssl ]]
  test "$(docker inspect --format '{{range .Config.Env}}{{println .}}{{end}}' "${name}" | sed -n 's/^MALLOC_CONF=//p')" \
    = 'narenas:1,retain:false,dirty_decay_ms:1000,tcache_max:8192'
  docker image inspect "${IMAGE}" | jq -e '.[0].Config.ExposedPorts["443/udp"] != null' >/dev/null
  [[ $(docker inspect --format '{{index .Config.Labels "org.opencontainers.image.rust.pgo"}}' "${name}") == "${EXPECTED_PGO}" ]]
  [[ $(docker inspect --format '{{index .Config.Labels "org.opencontainers.image.rust.linker"}}' "${name}") == lld ]]

  native_pgo=$(docker inspect --format '{{index .Config.Labels "org.opencontainers.image.native.pgo"}}' "${name}")
  case "${native_pgo}" in on|off) ;; *) echo "invalid native PGO label: ${native_pgo}" >&2; return 1 ;; esac
  if [[ "${EXPECTED_PGO}" == train ]]; then
    docker exec "${name}" sh -c 'test -s /usr/share/doc/pingora/pgo-profile-summary.txt'
    if [[ "${native_pgo}" == on ]]; then
      docker exec "${name}" sh -c 'test -s /usr/share/doc/pingora/pgo-native-profile-summary.txt'
    fi
  fi

  if docker exec "${name}" sh -c 'command -v setcap >/dev/null || dpkg-query -W libcap2-bin >/dev/null 2>&1'; then
    echo "runtime image unexpectedly contains libcap2-bin" >&2
    return 1
  fi
}

write_config "${RUNTIME}/http.yaml" '["127.0.0.1:80"]' '[]'
start_container pingora-test-http "${RUNTIME}/http.yaml"
assert_container_hardening pingora-test-http
curl --noproxy '*' -fsS -H 'host: health.invalid' \
  http://127.0.0.1:80/pingora-health -o /dev/null
[[ $(curl --noproxy '*' -sS -o /dev/null -w '%{http_code}' \
  -H 'host: health.invalid' http://127.0.0.1:80/nginx-health) == 404 ]]
docker rm -f pingora-test-http >/dev/null

write_config "${RUNTIME}/ipv6.yaml" '[]' '["[::1]:443"]' true '["[::1]:8443"]'
openssl req -x509 -newkey rsa:2048 -nodes -days 1 \
  -subj '/CN=health.test' -addext 'subjectAltName=DNS:health.test' \
  -keyout "${RUNTIME}/cert/key.pem" -out "${RUNTIME}/cert/cert.pem" >/dev/null 2>&1
chmod 0640 "${RUNTIME}/cert/key.pem" "${RUNTIME}/cert/cert.pem"
if [[ ${EUID} -eq 0 ]]; then
  chown 0:10001 "${RUNTIME}/cert/key.pem" "${RUNTIME}/cert/cert.pem"
elif command -v sudo >/dev/null 2>&1; then
  sudo chown 0:10001 "${RUNTIME}/cert/key.pem" "${RUNTIME}/cert/cert.pem"
else
  echo "root or passwordless sudo is required to create the root:10001 TLS fixture" >&2
  exit 1
fi
start_container pingora-test-https-ipv6 "${RUNTIME}/ipv6.yaml" \
  --volume "${RUNTIME}/cert:/etc/pingora/cert:ro"
assert_container_hardening pingora-test-https-ipv6
curl --noproxy '*' -gkfsS --http2 --resolve health.test:443:[::1] \
  https://health.test:443/pingora-ready -o /dev/null
docker logs pingora-test-https-ipv6 2>&1 \
  | grep -q 'HTTP/3 frontend started: udp=\["\[::1\]:8443"\]'

docker exec pingora-test-https-ipv6 /usr/local/bin/pingora \
  --config /etc/pingora/pingora.yaml --check >/dev/null

echo "Docker UID 10001, read-only filesystem, HTTP-only, HTTPS/HTTP3 IPv6-only, UDP exposure, healthcheck, ${EXPECTED_ALLOCATOR}, ${EXPECTED_TLS_PROVIDER}, and pgo=${EXPECTED_PGO} tests passed"
