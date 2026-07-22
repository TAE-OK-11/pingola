#!/usr/bin/env bash
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
ARTIFACTS=${1:?usage: smoke.sh ARTIFACT_DIR [OUTPUT_DIR]}
OUTPUT=${2:-${ROOT}/bench/results/candidate-smoke-$(date -u +%Y%m%dT%H%M%SZ)}
BACKEND_PORT=${BACKEND_PORT:-18700}
HTTP_PORT=${HTTP_PORT:-80}
HTTPS_PORT=${HTTPS_PORT:-443}
WORKERS=${BENCH_WORKERS:-$(nproc)}
BACKEND_PID=
PROXY_PID=

cleanup_proxy() {
  if [[ -n "${PROXY_PID}" ]]; then
    kill -TERM "${PROXY_PID}" >/dev/null 2>&1 || true
    for _ in {1..40}; do
      kill -0 "${PROXY_PID}" >/dev/null 2>&1 || break
      sleep 0.05
    done
    kill -KILL "${PROXY_PID}" >/dev/null 2>&1 || true
    wait "${PROXY_PID}" >/dev/null 2>&1 || true
    PROXY_PID=
  fi
}

cleanup() {
  cleanup_proxy
  if [[ -n "${BACKEND_PID}" ]]; then
    kill -TERM "${BACKEND_PID}" >/dev/null 2>&1 || true
    wait "${BACKEND_PID}" >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT INT TERM

for command in curl nproc openssl sha256sum; do
  command -v "${command}" >/dev/null || {
    echo "missing required command: ${command}" >&2
    exit 2
  }
done
for binary in \
  "${ARTIFACTS}/tools/backend" \
  "${ARTIFACTS}/jbs/pingora" \
  "${ARTIFACTS}/pingap/pingap" \
  "${ARTIFACTS}/aralez/aralez" \
  "${ARTIFACTS}/pingpong/pingpong" \
  "${ARTIFACTS}/zentinel/zentinel"; do
  [[ -x "${binary}" ]] || {
    echo "missing Actions-built binary: ${binary}" >&2
    exit 2
  }
done

install -d "${OUTPUT}/raw"
openssl req -x509 -newkey rsa:2048 -nodes -days 1 \
  -subj '/CN=bench.test' -addext 'subjectAltName=DNS:bench.test' \
  -keyout "${OUTPUT}/key.pem" -out "${OUTPUT}/cert.pem" >/dev/null 2>&1
chmod 0600 "${OUTPUT}/key.pem"

"${ARTIFACTS}/tools/backend" --port "${BACKEND_PORT}" \
  >"${OUTPUT}/backend.stdout" 2>"${OUTPUT}/backend.stderr" &
BACKEND_PID=$!
for _ in {1..100}; do
  curl --noproxy '*' -fsS "http://127.0.0.1:${BACKEND_PORT}/health" \
    -o /dev/null 2>/dev/null && break
  sleep 0.05
done
curl --noproxy '*' -fsS "http://127.0.0.1:${BACKEND_PORT}/bytes/64" \
  -o "${OUTPUT}/backend-64.body"
EXPECTED_SHA=$(sha256sum "${OUTPUT}/backend-64.body" | cut -d' ' -f1)

printf 'candidate\th1_status\th1_version\th1_sha256\th2_status\th2_version\th2_sha256\tresult\n' \
  >"${OUTPUT}/results.tsv"

for candidate in pingora pingap aralez pingpong zentinel; do
  config_dir=${OUTPUT}/${candidate}
  "${ROOT}/bench/candidates/configure.sh" \
    "${candidate}" "${config_dir}" "${HTTP_PORT}" "${HTTPS_PORT}" \
    "${BACKEND_PORT}" "${OUTPUT}/cert.pem" "${OUTPUT}/key.pem" "${WORKERS}"

  case "${candidate}" in
    pingora)
      command=("${ARTIFACTS}/jbs/pingora" --config "${config_dir}/pingora.yaml")
      ;;
    pingap)
      command=("${ARTIFACTS}/pingap/pingap" -c "${config_dir}/pingap.toml")
      ;;
    aralez)
      command=("${ARTIFACTS}/aralez/aralez" -c "${config_dir}/main.yaml")
      ;;
    pingpong)
      command=("${ARTIFACTS}/pingpong/pingpong" -c "${config_dir}/pingpong.toml")
      ;;
    zentinel)
      command=(env RUST_LOG=error "${ARTIFACTS}/zentinel/zentinel" -c "${config_dir}/zentinel.kdl")
      ;;
  esac

  "${command[@]}" >"${OUTPUT}/raw/${candidate}.stdout" \
    2>"${OUTPUT}/raw/${candidate}.stderr" &
  PROXY_PID=$!
  ready=false
  for _ in {1..200}; do
    if curl --noproxy '*' -fsS -H 'host: bench.test' \
      "http://127.0.0.1:${HTTP_PORT}/bytes/64" -o /dev/null 2>/dev/null; then
      ready=true
      break
    fi
    if ! kill -0 "${PROXY_PID}" >/dev/null 2>&1; then
      break
    fi
    sleep 0.05
  done

  result=PASS
  h1_meta='000 NA'
  h2_meta='000 NA'
  h1_sha=NA
  h2_sha=NA
  if [[ "${ready}" == true ]]; then
    set +e
    h1_meta=$(curl --noproxy '*' -sS --http1.1 -H 'host: bench.test' \
      -H 'accept-encoding: identity' -o "${OUTPUT}/raw/${candidate}.h1.body" \
      -w '%{http_code} %{http_version}' \
      "http://127.0.0.1:${HTTP_PORT}/bytes/64")
    h1_rc=$?
    h2_meta=$(curl --noproxy '*' -ksS --http2 \
      --resolve "bench.test:${HTTPS_PORT}:127.0.0.1" \
      -H 'accept-encoding: identity' -o "${OUTPUT}/raw/${candidate}.h2.body" \
      -w '%{http_code} %{http_version}' \
      "https://bench.test:${HTTPS_PORT}/bytes/64")
    h2_rc=$?
    set -e
    h1_sha=$(sha256sum "${OUTPUT}/raw/${candidate}.h1.body" | cut -d' ' -f1)
    h2_sha=$(sha256sum "${OUTPUT}/raw/${candidate}.h2.body" | cut -d' ' -f1)
    if ((h1_rc != 0 || h2_rc != 0)) \
      || [[ "${h1_meta}" != '200 1.1' || "${h2_meta%% *}" != '200' ]] \
      || [[ "${h1_sha}" != "${EXPECTED_SHA}" || "${h2_sha}" != "${EXPECTED_SHA}" ]]; then
      result=FAIL
    elif [[ "${h2_meta#* }" != 2 ]]; then
      result=UNSUPPORTED_H2
    fi
  else
    result=FAIL_START
  fi

  printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
    "${candidate}" "${h1_meta%% *}" "${h1_meta#* }" "${h1_sha}" \
    "${h2_meta%% *}" "${h2_meta#* }" "${h2_sha}" "${result}" \
    >>"${OUTPUT}/results.tsv"
  cleanup_proxy
done

cat "${OUTPUT}/results.tsv"
if grep -Eq $'\tFAIL(_START)?$' "${OUTPUT}/results.tsv"; then
  exit 1
fi
