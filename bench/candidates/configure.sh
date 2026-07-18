#!/usr/bin/env bash
set -euo pipefail

CANDIDATE=${1:?usage: configure.sh CANDIDATE OUTPUT_DIR HTTP_PORT HTTPS_PORT BACKEND_PORT CERT KEY WORKERS}
OUTPUT_DIR=${2:?missing output directory}
HTTP_PORT=${3:?missing HTTP port}
HTTPS_PORT=${4:?missing HTTPS port}
BACKEND_PORT=${5:?missing backend port}
CERT=${6:?missing certificate path}
KEY=${7:?missing private key path}
WORKERS=${8:?missing worker count}

for value in "${HTTP_PORT}" "${HTTPS_PORT}" "${BACKEND_PORT}" "${WORKERS}"; do
  [[ "${value}" =~ ^[1-9][0-9]*$ ]] || {
    echo "ports and workers must be positive integers: ${value}" >&2
    exit 2
  }
done
[[ "${CERT}" = /* && "${KEY}" = /* ]] || {
  echo "certificate and private key paths must be absolute" >&2
  exit 2
}
[[ -r "${CERT}" && -r "${KEY}" ]] || {
  echo "certificate or private key is not readable: cert=${CERT} key=${KEY}" >&2
  exit 2
}

install -d "${OUTPUT_DIR}"

case "${CANDIDATE}" in
  pingora)
    cat >"${OUTPUT_DIR}/pingora.yaml" <<EOF
server:
  http_listen: ["127.0.0.1:${HTTP_PORT}"]
  https_listen: ["127.0.0.1:${HTTPS_PORT}"]
  certificate: ${CERT}
  private_key: ${KEY}
  health_socket: ${OUTPUT_DIR}/pingora-health.sock
  threads: ${WORKERS}
  upstream_keepalive_pool_size: 128
  downstream_keepalive_requests: 1000000
  max_retries: 0
  access_log: false
  health_details: false
  http2_max_concurrent_streams: 128
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
  bench:
    domains: ["bench.test"]
    handler: vaultwarden
    upstream: backend
    max_body_bytes: 536870912
route_limits:
  vaultwarden:
    rate_per_second: 0
    active_requests: 0
EOF
    ;;
  pingap)
    cat >"${OUTPUT_DIR}/pingap.toml" <<EOF
[basic]
threads = ${WORKERS}
upstream_keepalive_pool_size = 128
log_level = "error"

[locations.bench]
upstream = "backend"
path = "/"
host = "bench.test"

[servers.http]
addr = "127.0.0.1:${HTTP_PORT}"
locations = ["bench"]
threads = ${WORKERS}

[servers.https]
addr = "127.0.0.1:${HTTPS_PORT}"
locations = ["bench"]
threads = ${WORKERS}
global_certificates = true
enabled_h2 = true
tls_min_version = "tls1.3"
tls_max_version = "tls1.3"

[upstreams.backend]
addrs = ["127.0.0.1:${BACKEND_PORT}"]
alpn = "h1"
idle_timeout = "30s"
connection_timeout = "2s"
read_timeout = "60s"
write_timeout = "60s"

[certificates.bench]
domains = "bench.test"
tls_cert = "${CERT}"
tls_key = "${KEY}"
is_default = true
EOF
    ;;
  pingpong)
    cat >"${OUTPUT_DIR}/pingpong.toml" <<EOF
pid_file = "${OUTPUT_DIR}/pingpong.pid"
upgrade_sock = "${OUTPUT_DIR}/pingpong.sock"
threads = ${WORKERS}
work_stealing = true
log = "/dev/null"

[server.${HTTP_PORT}]
threads = ${WORKERS}

[server.${HTTP_PORT}.source.backend]
ip = "127.0.0.1"
port = ${BACKEND_PORT}
ssl = false
location = ["/"]

[server.${HTTPS_PORT}]
threads = ${WORKERS}
ssl = { cert = "${CERT}", key = "${KEY}" }

[server.${HTTPS_PORT}.source.backend]
ip = "127.0.0.1"
port = ${BACKEND_PORT}
ssl = false
location = ["/"]
EOF
    ;;
  aralez)
    install -d "${OUTPUT_DIR}/certificates"
    install -m0644 "${CERT}" "${OUTPUT_DIR}/certificates/bench.test.crt"
    install -m0600 "${KEY}" "${OUTPUT_DIR}/certificates/bench.test.key"
    cat >"${OUTPUT_DIR}/main.yaml" <<EOF
pid_file: ${OUTPUT_DIR}/aralez.pid
upgrade_sock: ${OUTPUT_DIR}/aralez.sock
config_api_enabled: false
config_address: 127.0.0.1:18999
proxy_address_http: 127.0.0.1:${HTTP_PORT}
proxy_address_tls: 127.0.0.1:${HTTPS_PORT}
proxy_configs: ${OUTPUT_DIR}
proxy_tls_grade: high
upstreams_conf: ${OUTPUT_DIR}/upstreams.yaml
log_level: error
access_log: none
hc_method: GET
hc_interval: 30
EOF
    cat >"${OUTPUT_DIR}/upstreams.yaml" <<EOF
provider: file
upstreams:
  bench.test:
    paths:
      "/":
        healthcheck: false
        servers:
          - "127.0.0.1:${BACKEND_PORT}"
EOF
    ;;
  zentinel)
    cat >"${OUTPUT_DIR}/zentinel.kdl" <<EOF
system {
    worker-threads ${WORKERS}
    max-connections 10000
    graceful-shutdown-timeout-secs 2
}

listeners {
    listener "http" {
        address "127.0.0.1:${HTTP_PORT}"
        protocol "http"
        request-timeout-secs 60
        keepalive-timeout-secs 30
    }
    listener "https" {
        address "127.0.0.1:${HTTPS_PORT}"
        protocol "https"
        request-timeout-secs 60
        keepalive-timeout-secs 30
        tls {
            cert-file "${CERT}"
            key-file "${KEY}"
            min-version "TLS1.3"
            session-resumption #true
        }
    }
}

routes {
    route "bench" {
        priority "normal"
        matches {
            path-prefix "/"
            host "bench.test"
        }
        upstream "backend"
        policies {
            timeout-secs 60
            failure-mode "open"
        }
    }
}

upstreams {
    upstream "backend" {
        target "127.0.0.1:${BACKEND_PORT}" weight=1
        load-balancing "round_robin"
        connection-pool {
            max-connections 128
            max-idle 128
            idle-timeout-secs 30
        }
        timeouts {
            connect-secs 2
            request-secs 60
            read-secs 60
            write-secs 60
        }
    }
}

limits {
    max-header-size-bytes 8192
    max-header-count 100
    max-body-size-bytes 536870912
    max-connections-per-client 10000
    max-connections-per-route 10000
    max-total-connections 10000
    max-idle-connections-per-upstream 128
    max-in-flight-requests 10000
    max-in-flight-requests-per-worker 10000
    max-queued-requests 1000
    max-requests-per-second-global 1000000
    max-requests-per-second-per-client 1000000
}

observability {
    metrics {
        enabled #false
    }
    logging {
        level "error"
        format "pretty"
        timestamps #false
        access-log {
            enabled #false
        }
        error-log {
            enabled #false
        }
        audit-log {
            enabled #false
        }
    }
}
EOF
    ;;
  *)
    echo "unsupported candidate: ${CANDIDATE}" >&2
    exit 2
    ;;
esac
