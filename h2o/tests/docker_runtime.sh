#!/usr/bin/env bash
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
IMAGE=${H2O_TEST_IMAGE:-ghcr.io/tae-ok-11/pingora/h2o:local}
EXPECTED_TARGET_CPU=${H2O_EXPECTED_TARGET_CPU:-x86-64-v2}
EXPECTED_LTO=${H2O_EXPECTED_LTO:-fat}
RUNTIME=${H2O_DOCKER_TEST_RUNTIME:-/tmp/h2o-docker-runtime}
CONTAINERS=()

cleanup() {
  if ((${#CONTAINERS[@]})); then
    docker rm -f "${CONTAINERS[@]}" >/dev/null 2>&1 || true
  fi
  rm -rf "${RUNTIME}"
}
trap cleanup EXIT INT TERM

rm -rf "${RUNTIME}"
install -d -m 0755 "${RUNTIME}"

write_config() {
  local file=$1
  cat >"${file}" <<'EOF'
num-threads: 1
temp-buffer-path: /tmp/h2o
listen:
  - host: 127.0.0.1
    port: 80
hosts:
  "health.invalid":
    paths:
      /h2o-health:
        file.dir: /usr/share/h2o/health
      /pingora-health:
        file.dir: /usr/share/h2o/health
  "app.test":
    paths:
      /:
        proxy.reverse.url: http://127.0.0.1:9
EOF
  chmod 0644 "${file}"
}

start_container() {
  local name=$1
  local config=$2

  CONTAINERS+=("${name}")
  docker run --detach --name "${name}" --network host \
    --read-only --cap-drop ALL --cap-add NET_BIND_SERVICE \
    --security-opt no-new-privileges \
    --health-start-period 0s --health-interval 1s --health-timeout 2s --health-retries 10 \
    --tmpfs /tmp/h2o:rw,noexec,nosuid,nodev,uid=10001,gid=10001,mode=0770 \
    --volume "${config}:/etc/h2o/h2o.conf:ro" \
    "${IMAGE}" -c /etc/h2o/h2o.conf -m worker >/dev/null

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
  local name=$1

  [[ $(docker inspect --format '{{.Config.User}}' "${name}") == 10001:10001 ]]
  [[ $(docker inspect --format '{{.HostConfig.ReadonlyRootfs}}' "${name}") == true ]]
  [[ $(docker inspect --format '{{json .HostConfig.CapDrop}}' "${name}") == '["ALL"]' ]]
  [[ $(docker inspect --format '{{json .HostConfig.CapAdd}}' "${name}") == '["CAP_NET_BIND_SERVICE"]' ]]
  [[ $(docker inspect --format '{{index .Config.Labels "org.opencontainers.image.h2o.target-cpu"}}' "${name}") == "${EXPECTED_TARGET_CPU}" ]]
  [[ $(docker inspect --format '{{index .Config.Labels "org.opencontainers.image.h2o.lto"}}' "${name}") == "${EXPECTED_LTO}" ]]
  [[ $(docker inspect --format '{{index .Config.Labels "org.opencontainers.image.h2o.linker"}}' "${name}") == lld ]]
  docker image inspect "${IMAGE}" | jq -e '.[0].Config.ExposedPorts["443/udp"] != null' >/dev/null
  docker exec "${name}" test -s /usr/share/doc/h2o/version.txt

  if docker exec "${name}" sh -c 'command -v setcap >/dev/null || dpkg-query -W libcap2-bin >/dev/null 2>&1'; then
    echo "runtime image unexpectedly contains libcap2-bin" >&2
    return 1
  fi
}

write_config "${RUNTIME}/http.yaml"
start_container h2o-test-http "${RUNTIME}/http.yaml"
assert_container_hardening h2o-test-http
curl --noproxy '*' -fsS -H 'host: health.invalid' \
  http://127.0.0.1:80/h2o-health -o /dev/null
curl --noproxy '*' -fsS -H 'host: health.invalid' \
  http://127.0.0.1:80/pingora-health -o /dev/null
docker exec h2o-test-http /usr/local/bin/h2o -c /etc/h2o/h2o.conf -m test >/dev/null

echo "Docker UID 10001, read-only filesystem, healthcheck, ${EXPECTED_LTO} LTO, and config validation tests passed"
