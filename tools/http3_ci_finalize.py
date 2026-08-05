#!/usr/bin/env python3
from pathlib import Path


def replace(path: str, old: str, new: str, count: int = 1) -> None:
    file = Path(path)
    text = file.read_text()
    actual = text.count(old)
    if actual != count:
        raise SystemExit(f"{path}: expected {count} matches, found {actual}: {old!r}")
    file.write_text(text.replace(old, new))


replace(
    ".github/workflows/ci.yml",
    "sudo apt-get install --yes cmake libcap2-bin ninja-build jq nghttp2-client",
    "sudo apt-get install --yes clang cmake libcap2-bin ninja-build jq nghttp2-client openssl perl",
)
replace(
    ".github/workflows/ci.yml",
    '''      - name: AWS-LC-only TLS dependency graph
        run: |
          cargo tree --locked > /tmp/cargo-tree.txt
          grep -q 'aws-lc-rs v' /tmp/cargo-tree.txt
          grep -q 'rustls v' /tmp/cargo-tree.txt
          if grep -Eiq '(^|[[:space:]])(boring|boring-sys|pingora-boringssl) v' /tmp/cargo-tree.txt; then
            echo 'BoringSSL unexpectedly appears in the selected dependency graph' >&2
            exit 1
          fi
''',
    '''      - name: TLS provider dependency boundaries
        run: |
          cargo tree --locked > /tmp/cargo-tree.txt
          cargo tree --locked -p pingora-rustls@0.8.1 > /tmp/pingora-tls-tree.txt
          cargo tree --locked -p tokio-quiche@0.19.1 > /tmp/http3-tls-tree.txt

          grep -q 'aws-lc-rs v1.17.3' /tmp/pingora-tls-tree.txt
          grep -q 'rustls v0.23.43' /tmp/pingora-tls-tree.txt
          if grep -Eiq '(^|[[:space:]])(boring|boring-sys|pingora-boringssl) v' /tmp/pingora-tls-tree.txt; then
            echo 'BoringSSL unexpectedly appears in the Pingora TCP TLS dependency tree' >&2
            exit 1
          fi

          grep -q 'tokio-quiche v0.19.1' /tmp/http3-tls-tree.txt
          grep -q 'quiche v0.29.3' /tmp/http3-tls-tree.txt
          grep -q 'boring v4.22.0' /tmp/http3-tls-tree.txt
          grep -q 'boring-sys v4.22.0' /tmp/http3-tls-tree.txt
          if grep -q 'pingora-boringssl v' /tmp/cargo-tree.txt; then
            echo 'Pingora must not select its BoringSSL TLS adapter' >&2
            exit 1
          fi
''',
)
replace(
    ".github/workflows/ci.yml",
    '''          tests/service_matrix.sh
          tests/integration.sh
''',
    '''          tests/service_matrix.sh
          tests/integration.sh
          tests/http3.sh
''',
)

replace(
    "Dockerfile",
    '''      org.opencontainers.image.tls.provider="${TLS_PROVIDER}" \
      org.opencontainers.image.rust.pgo="${PGO_MODE}" \
''',
    '''      org.opencontainers.image.tls.provider="${TLS_PROVIDER}" \
      org.opencontainers.image.http3.provider="quiche" \
      org.opencontainers.image.quic.tls.provider="boringssl" \
      org.opencontainers.image.rust.pgo="${PGO_MODE}" \
''',
)
replace(
    "Dockerfile",
    "EXPOSE 80/tcp 443/tcp",
    "EXPOSE 80/tcp 443/tcp 443/udp",
)

replace(
    "tests/docker_runtime.sh",
    '''  local tls=${4:-false}
  {
''',
    '''  local tls=${4:-false}
  local http3=${5:-'[]'}
  {
''',
)
replace(
    "tests/docker_runtime.sh",
    '''    printf '  https_listen: %s\\n' "${https}"
    if [[ "${tls}" == true ]]; then
''',
    '''    printf '  https_listen: %s\\n' "${https}"
    printf '  http3_listen: %s\\n' "${http3}"
    if [[ "${http3}" != '[]' ]]; then
      printf '  http3_internal_listen: "127.0.0.1:18080"\\n'
      printf '  http3_max_idle_timeout_seconds: 15\\n'
      printf '  http3_max_concurrent_streams: 16\\n'
    fi
    if [[ "${tls}" == true ]]; then
''',
)
replace(
    "tests/docker_runtime.sh",
    '''  [[ $(docker inspect --format '{{index .Config.Labels "org.opencontainers.image.tls.provider"}}' "${name}") == "${EXPECTED_TLS_PROVIDER}" ]]
  [[ $(docker inspect --format '{{index .Config.Labels "org.opencontainers.image.rust.pgo"}}' "${name}") == "${EXPECTED_PGO}" ]]
''',
    '''  [[ $(docker inspect --format '{{index .Config.Labels "org.opencontainers.image.tls.provider"}}' "${name}") == "${EXPECTED_TLS_PROVIDER}" ]]
  [[ $(docker inspect --format '{{index .Config.Labels "org.opencontainers.image.http3.provider"}}' "${name}") == quiche ]]
  [[ $(docker inspect --format '{{index .Config.Labels "org.opencontainers.image.quic.tls.provider"}}' "${name}") == boringssl ]]
  docker image inspect "${IMAGE}" | jq -e '.[0].Config.ExposedPorts["443/udp"] != null' >/dev/null
  [[ $(docker inspect --format '{{index .Config.Labels "org.opencontainers.image.rust.pgo"}}' "${name}") == "${EXPECTED_PGO}" ]]
''',
)
replace(
    "tests/docker_runtime.sh",
    '''write_config "${RUNTIME}/ipv6.yaml" '[]' '["[::1]:443"]' true
''',
    '''write_config "${RUNTIME}/ipv6.yaml" '[]' '["[::1]:443"]' true '["[::1]:8443"]'
''',
)
replace(
    "tests/docker_runtime.sh",
    '''curl --noproxy '*' -gkfsS --http2 --resolve health.test:443:[::1] \
  https://health.test:443/pingora-ready -o /dev/null

docker exec pingora-test-https-ipv6 /usr/local/bin/pingora \
''',
    '''curl --noproxy '*' -gkfsS --http2 --resolve health.test:443:[::1] \
  https://health.test:443/pingora-ready -o /dev/null
docker logs pingora-test-https-ipv6 2>&1 \
  | grep -q 'HTTP/3 frontend started: udp=\\["\\[::1\\]:8443"\\]'

docker exec pingora-test-https-ipv6 /usr/local/bin/pingora \
''',
)
replace(
    "tests/docker_runtime.sh",
    '''echo "Docker UID 10001, read-only filesystem, HTTP-only, HTTPS-only, IPv6-only, healthcheck, ${EXPECTED_ALLOCATOR}, ${EXPECTED_TLS_PROVIDER}, and pgo=${EXPECTED_PGO} tests passed"
''',
    '''echo "Docker UID 10001, read-only filesystem, HTTP-only, HTTPS/HTTP3 IPv6-only, UDP exposure, healthcheck, ${EXPECTED_ALLOCATOR}, ${EXPECTED_TLS_PROVIDER}, and pgo=${EXPECTED_PGO} tests passed"
''',
)
