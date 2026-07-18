#!/usr/bin/env bash
set -Eeuo pipefail

BASE_IMAGE=${BASE_IMAGE:-ghcr.io/tae-ok-11/pingora@sha256:b84540309b6e3dbdfdfff2f68c175df1b69c068435242acb77bf9eed5b4d9b59}
OLD_PGO_IMAGE=${OLD_PGO_IMAGE:-ghcr.io/tae-ok-11/pingora@sha256:b3a053f335ce916ca7a3e239b9c8ff7e8f3ef543c4fffa2e4eec3fbd776b3d3d}
NEW_PGO_IMAGE=${NEW_PGO_IMAGE:-ghcr.io/tae-ok-11/pingora:pgo-znver1}
OHA_IMAGE=${OHA_IMAGE:-ghcr.io/hatoo/oha:1.15}

PINGORA_REPO=${PINGORA_REPO:-/root/pingola}
OUTPUT=${BENCH_OUTPUT:-/root/pingora-3way-$(date -u +%Y%m%dT%H%M%SZ)}
MEMORY=${MEMORY:-1g}
MEMORY_SWAP=${MEMORY_SWAP:-1g}
PROBE_RUNS=${PROBE_RUNS:-5}
LATENCY_RUNS=${LATENCY_RUNS:-7}
PROBE_SECONDS=${PROBE_SECONDS:-12}
WARMUP_SECONDS=${WARMUP_SECONDS:-4}
TEST_SECONDS=${TEST_SECONDS:-30}
LOAD_FACTOR=${LOAD_FACTOR:-0.65}
COOLDOWN_SECONDS=${COOLDOWN_SECONDS:-1}
MIN_FREE_BYTES=${MIN_FREE_BYTES:-1073741824}
SUT_CPU=${SUT_CPU:-0}
BACKEND_CPU=${BACKEND_CPU:-1}
CLIENT_CPU=${CLIENT_CPU:-}
NUMA_NODE=${NUMA_NODE:-0}
BACKEND_PORT=${BACKEND_PORT:-18800}
HTTP_PORT=${HTTP_PORT:-18880}
HTTPS_PORT=${HTTPS_PORT:-18843}
BENCH_HOST=${BENCH_HOST:-profile.test}

RAW=${OUTPUT}/raw
PROBES=${OUTPUT}/probes.tsv
PROBE_SUMMARY=${OUTPUT}/probe-summary.tsv
MEASUREMENTS=${OUTPUT}/measurements.tsv
SUMMARY=${OUTPUT}/summary.txt
BACKEND_BIN=${OUTPUT}/backend-rust
BACKEND_PID=
SUT_NAME=
SUT_SEQUENCE=0

declare -A IMAGES=(
  [baseline]="${BASE_IMAGE}"
  [old-pgo]="${OLD_PGO_IMAGE}"
  [new-pgo]="${NEW_PGO_IMAGE}"
)

log(){ printf '\n[%s] %s\n' "$(date +%H:%M:%S)" "$*" >&2; }
die(){ echo "ERROR: $*" >&2; exit 1; }

cleanup_sut(){
  local stopped=false
  if [[ -n "${SUT_NAME}" ]] && docker inspect "${SUT_NAME}" >/dev/null 2>&1; then
    docker logs "${SUT_NAME}" >"${OUTPUT}/${SUT_NAME}.log" 2>&1 || true
    docker rm -f "${SUT_NAME}" >/dev/null 2>&1 || true
    stopped=true
  fi
  SUT_NAME=
  if [[ "${stopped}" == true ]] && [[ "${COOLDOWN_SECONDS}" != 0 ]]; then
    sleep "${COOLDOWN_SECONDS}"
  fi
}
cleanup(){
  cleanup_sut
  if [[ -n "${BACKEND_PID}" ]]; then
    kill "${BACKEND_PID}" 2>/dev/null || true
    wait "${BACKEND_PID}" 2>/dev/null || true
  fi
}
handle_signal(){
  trap - EXIT INT TERM
  cleanup
  exit 130
}
trap cleanup EXIT
trap handle_signal INT TERM

for c in docker curl jq numfmt openssl rustc sha256sum taskset awk sort lscpu; do
  command -v "$c" >/dev/null || die "missing command: $c"
done
docker info >/dev/null 2>&1 || die "Docker unavailable"
[[ -f "${PINGORA_REPO}/bench/backend.rs" ]] || die "backend.rs not found"

CPU_COUNT=$(nproc)
((CPU_COUNT >= 2)) || die "at least 2 CPUs required"
if [[ -z "${CLIENT_CPU}" ]]; then
  if ((CPU_COUNT >= 3)); then CLIENT_CPU=2; else CLIENT_CPU=${BACKEND_CPU}; fi
fi
for cpu in "${SUT_CPU}" "${BACKEND_CPU}" "${CLIENT_CPU}"; do
  [[ "$cpu" =~ ^[0-9]+$ ]] || die "invalid CPU: $cpu"
  ((cpu < CPU_COUNT)) || die "CPU $cpu unavailable; nproc=$CPU_COUNT"
done
[[ "${SUT_CPU}" != "${BACKEND_CPU}" ]] || die "SUT/backend CPU must differ"
[[ "${SUT_CPU}" != "${CLIENT_CPU}" ]] || die "SUT/client CPU must differ"

mkdir -p "${OUTPUT}" "${RAW}"
chmod 755 "${OUTPUT}"
AVAILABLE_BYTES=$(df --output=avail -B1 "${OUTPUT}" | tail -1 | tr -d ' ')
((AVAILABLE_BYTES >= MIN_FREE_BYTES)) || \
  die "insufficient disk: available=${AVAILABLE_BYTES} required=${MIN_FREE_BYTES}"

log "3개 이미지 pull"
docker pull "${BASE_IMAGE}"
docker pull "${OLD_PGO_IMAGE}"
docker pull "${NEW_PGO_IMAGE}"
docker pull "${OHA_IMAGE}"

BASE_ID=$(docker image inspect -f '{{.Id}}' "${BASE_IMAGE}")
OLD_ID=$(docker image inspect -f '{{.Id}}' "${OLD_PGO_IMAGE}")
NEW_ID=$(docker image inspect -f '{{.Id}}' "${NEW_PGO_IMAGE}")
OHA_ID=$(docker image inspect -f '{{.Id}}' "${OHA_IMAGE}")
[[ "$BASE_ID" != "$OLD_ID" ]] || die "baseline=old-pgo image ID"
[[ "$BASE_ID" != "$NEW_ID" ]] || die "baseline=new-pgo image ID"
[[ "$OLD_ID" != "$NEW_ID" ]] || die "pgo-znver1 tag is still the OLD PGO image; wait for Actions and pull again"

digest(){
  docker image inspect "$1" | jq -r '.[0].RepoDigests[]? | select(startswith("ghcr.io/tae-ok-11/pingora@"))' | head -n1
}
image_digest(){
  docker image inspect "$1" | jq -r '.[0].RepoDigests[0] // empty'
}
BASE_DIGEST=$(digest "${BASE_IMAGE}")
OLD_DIGEST=$(digest "${OLD_PGO_IMAGE}")
NEW_DIGEST=$(digest "${NEW_PGO_IMAGE}")
OHA_DIGEST=$(image_digest "${OHA_IMAGE}")
printf 'baseline=%s\nold-pgo=%s\nnew-pgo=%s\n' \
  "${BASE_DIGEST:-$BASE_ID}" "${OLD_DIGEST:-$OLD_ID}" "${NEW_DIGEST:-$NEW_ID}" >&2

printf 'image\treference\tdigest\trevision\tallocator\ttls_provider\ttarget_cpu\tpgo\n' \
  >"${OUTPUT}/images.tsv"
for key in baseline old-pgo new-pgo; do
  image=${IMAGES[$key]}
  docker image inspect "${image}" >"${OUTPUT}/${key}-inspect.json"
  allocator=$(docker image inspect -f '{{index .Config.Labels "org.opencontainers.image.allocator"}}' "${image}")
  tls=$(docker image inspect -f '{{index .Config.Labels "org.opencontainers.image.tls.provider"}}' "${image}")
  target_cpu=$(docker image inspect -f '{{index .Config.Labels "org.opencontainers.image.rust.target-cpu"}}' "${image}")
  pgo=$(docker image inspect -f '{{index .Config.Labels "org.opencontainers.image.rust.pgo"}}' "${image}")
  revision=$(docker image inspect -f '{{index .Config.Labels "org.opencontainers.image.revision"}}' "${image}")
  [[ "${allocator}" == tcmalloc ]] || die "${key} allocator=${allocator}, expected=tcmalloc"
  [[ "${tls}" == aws-lc ]] || die "${key} TLS provider=${tls}, expected=aws-lc"
  docker run --rm --entrypoint /usr/local/bin/pingora "${image}" --allocator-info \
    >"${OUTPUT}/${key}-allocator.txt"
  grep -q '^allocator=tcmalloc ' "${OUTPUT}/${key}-allocator.txt" || \
    die "${key} process is not using TCMalloc"
  printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
    "${key}" "${image}" "$(image_digest "${image}")" "${revision}" \
    "${allocator}" "${tls}" "${target_cpu}" "${pgo}" >>"${OUTPUT}/images.tsv"
done
docker image inspect "${OHA_IMAGE}" >"${OUTPUT}/oha-inspect.json"

log "backend 컴파일"
rustc --edition=2021 -D warnings -C opt-level=3 -C codegen-units=1 \
  -C panic=abort -C target-cpu=native -C strip=symbols \
  "${PINGORA_REPO}/bench/backend.rs" -o "${BACKEND_BIN}"

openssl req -x509 -newkey rsa:2048 -nodes -days 1 -sha256 \
  -subj "/CN=${BENCH_HOST}" -addext "subjectAltName=DNS:${BENCH_HOST}" \
  -keyout "${OUTPUT}/key.pem" -out "${OUTPUT}/cert.pem" >/dev/null 2>&1
chown 0:10001 "${OUTPUT}/key.pem" "${OUTPUT}/cert.pem"
chmod 0640 "${OUTPUT}/key.pem" "${OUTPUT}/cert.pem"

cat >"${OUTPUT}/pingora.yaml" <<EOF
server:
  http_listen: ["127.0.0.1:${HTTP_PORT}"]
  https_listen: ["127.0.0.1:${HTTPS_PORT}"]
  certificate: /work/cert.pem
  private_key: /work/key.pem
  health_socket: /tmp/pingora/health.sock
  health_details: true
  threads: 1
  upstream_keepalive_pool_size: 128
  downstream_keepalive_requests: 500
  max_retries: 0
  access_log: false
  http2_max_concurrent_streams: 128
  graceful_shutdown_timeout_seconds: 2
trusted_proxies: ["127.0.0.0/8"]
upstreams:
  backend:
    address: "127.0.0.1:${BACKEND_PORT}"
    protocol: http1
    connect_timeout_seconds: 2
    read_timeout_seconds: 30
    write_timeout_seconds: 30
    idle_timeout_seconds: 30
hosts:
  profile:
    domains: ["${BENCH_HOST}"]
    handler: vaultwarden
    upstream: backend
route_limits:
  vaultwarden:
    rate_per_second: 0
    active_requests: 0
EOF
chmod 0644 "${OUTPUT}/pingora.yaml"

log "backend CPU ${BACKEND_CPU}에서 시작"
taskset -c "${BACKEND_CPU}" "${BACKEND_BIN}" --port "${BACKEND_PORT}" \
  >"${OUTPUT}/backend.stdout" 2>"${OUTPUT}/backend.stderr" &
BACKEND_PID=$!
for _ in {1..160}; do
  curl --noproxy '*' -fsS "http://127.0.0.1:${BACKEND_PORT}/bytes/64" -o /dev/null 2>/dev/null && break
  kill -0 "${BACKEND_PID}" 2>/dev/null || die "backend exited"
  sleep .05
done
curl --noproxy '*' -fsS "http://127.0.0.1:${BACKEND_PORT}/bytes/64" -o /dev/null || die "backend readiness failed"
[[ "$(awk '/Cpus_allowed_list/{print $2}' /proc/${BACKEND_PID}/status)" == "${BACKEND_CPU}" ]] || die "backend affinity mismatch"
BACKEND_SHA_64=$(curl --noproxy '*' -fsS "http://127.0.0.1:${BACKEND_PORT}/bytes/64" | sha256sum | awk '{print $1}')
BACKEND_SHA_4096=$(curl --noproxy '*' -fsS "http://127.0.0.1:${BACKEND_PORT}/bytes/4096" | sha256sum | awk '{print $1}')
printf 'sequence\timage\tprotocol\tpayload_bytes\tsha256\tstatus\n' >"${OUTPUT}/body-checks.tsv"

verify_bodies(){
  local key=$1 protocol size expected output actual
  for protocol in h1 h2; do
    for size in 64 4096; do
      [[ "${size}" == 64 ]] && expected=${BACKEND_SHA_64} || expected=${BACKEND_SHA_4096}
      output="${RAW}/body-${SUT_SEQUENCE}-${key}-${protocol}-${size}.bin"
      if [[ "${protocol}" == h1 ]]; then
        curl --noproxy '*' --http1.1 --insecure --fail --silent --show-error \
          --resolve "${BENCH_HOST}:${HTTPS_PORT}:127.0.0.1" \
          "https://${BENCH_HOST}:${HTTPS_PORT}/bytes/${size}" -o "${output}"
      else
        curl --noproxy '*' --http2 --insecure --fail --silent --show-error \
          --resolve "${BENCH_HOST}:${HTTPS_PORT}:127.0.0.1" \
          "https://${BENCH_HOST}:${HTTPS_PORT}/bytes/${size}" -o "${output}"
      fi
      actual=$(sha256sum "${output}" | awk '{print $1}')
      [[ "${actual}" == "${expected}" ]] || \
        die "body mismatch: image=${key} protocol=${protocol} bytes=${size} expected=${expected} actual=${actual}"
      printf '%s\t%s\t%s\t%s\t%s\tPASS\n' \
        "${SUT_SEQUENCE}" "${key}" "${protocol}" "${size}" "${actual}" \
        >>"${OUTPUT}/body-checks.tsv"
    done
  done
}

start_sut(){
  local key=$1 image=${IMAGES[$1]} numa=()
  cleanup_sut
  SUT_SEQUENCE=$((SUT_SEQUENCE + 1))
  SUT_NAME="pingora-3way-${SUT_SEQUENCE}-${key}-$$"
  [[ -d /sys/devices/system/node/node${NUMA_NODE} ]] && numa=(--cpuset-mems "${NUMA_NODE}")
  docker run -d --name "${SUT_NAME}" --network host --read-only \
    --cpuset-cpus "${SUT_CPU}" "${numa[@]}" \
    --memory "${MEMORY}" --memory-swap "${MEMORY_SWAP}" --pids-limit 512 \
    --cap-drop ALL --cap-add NET_BIND_SERVICE --security-opt no-new-privileges \
    --tmpfs /tmp/pingora:rw,noexec,nosuid,nodev,size=16m,uid=10001,gid=10001,mode=0700 \
    -v "${OUTPUT}:/work:ro" --entrypoint /usr/local/bin/pingora \
    "${image}" --config /work/pingora.yaml >/dev/null
  local ok=false
  for _ in {1..160}; do
    if curl --noproxy '*' -fsS -H "host: ${BENCH_HOST}" "http://127.0.0.1:${HTTP_PORT}/bytes/64" -o /dev/null 2>/dev/null; then ok=true; break; fi
    docker inspect -f '{{.State.Running}}' "${SUT_NAME}" 2>/dev/null | grep -qx true || { docker logs "${SUT_NAME}" >&2 || true; die "$key exited"; }
    sleep .05
  done
  [[ "$ok" == true ]] || die "$key readiness failed"
  local pid allowed nano memory memory_swap expected_memory expected_swap cg quota period
  pid=$(docker inspect -f '{{.State.Pid}}' "${SUT_NAME}")
  allowed=$(awk '/Cpus_allowed_list/{print $2}' /proc/${pid}/status)
  nano=$(docker inspect -f '{{.HostConfig.NanoCpus}}' "${SUT_NAME}")
  memory=$(docker inspect -f '{{.HostConfig.Memory}}' "${SUT_NAME}")
  memory_swap=$(docker inspect -f '{{.HostConfig.MemorySwap}}' "${SUT_NAME}")
  expected_memory=$(numfmt --from=iec "${MEMORY^^}")
  expected_swap=$(numfmt --from=iec "${MEMORY_SWAP^^}")
  [[ "$allowed" == "${SUT_CPU}" ]] || die "$key affinity=$allowed"
  [[ "$nano" -eq 0 ]] || die "$key CFS NanoCpus=$nano"
  [[ "$memory" -eq "$expected_memory" ]] || die "$key memory=$memory expected=$expected_memory"
  [[ "$memory_swap" -eq "$expected_swap" ]] || die "$key memory-swap=$memory_swap expected=$expected_swap"
  cg=$(awk -F: '$1=="0"{print $3}' /proc/${pid}/cgroup)
  if [[ -r /sys/fs/cgroup${cg}/cpu.max ]]; then
    read -r quota period </sys/fs/cgroup${cg}/cpu.max
    [[ "$quota" == max ]] || die "$key cpu.max=$quota $period"
  fi
  docker inspect "${SUT_NAME}" >"${RAW}/sut-${SUT_SEQUENCE}-${key}-inspect.json"
  verify_bodies "${key}"
  log "$key: CPU ${SUT_CPU} 고정 / CFS quota 없음"
}

oha(){
  local numa=()
  [[ -d /sys/devices/system/node/node${NUMA_NODE} ]] && numa=(--cpuset-mems "${NUMA_NODE}")
  docker run --rm --network host --cpuset-cpus "${CLIENT_CPU}" "${numa[@]}" "${OHA_IMAGE}" "$@"
}

proto_args(){ [[ "$1" == h1 ]] && printf '%s\0' --http-version 1.1 || printf '%s\0' --http2 -p 8; }
connections(){ [[ "$1" == h1 ]] && echo 32 || echo 8; }
run_probe(){
  local p=$1 sec=$2 out=$3 c; local -a pa=()
  c=$(connections "$p"); mapfile -d '' -t pa < <(proto_args "$p")
  oha -z "${sec}s" --wait-ongoing-requests-after-deadline -c "$c" -t 10s --connect-timeout 5s \
    --no-tui --output-format json --stats-success-breakdown --redirect 0 --disable-compression \
    --insecure --connect-to "${BENCH_HOST}:${HTTPS_PORT}:127.0.0.1:${HTTPS_PORT}" "${pa[@]}" \
    "https://${BENCH_HOST}:${HTTPS_PORT}/bytes/64" >"$out"
}
run_fixed(){
  local p=$1 qps=$2 sec=$3 out=$4 c n; local -a pa=()
  c=$(connections "$p"); n=$((qps*sec)); mapfile -d '' -t pa < <(proto_args "$p")
  oha -n "$n" -q "$qps" -c "$c" -t 10s --connect-timeout 5s \
    --no-tui --output-format json --stats-success-breakdown --redirect 0 --disable-compression \
    --insecure --connect-to "${BENCH_HOST}:${HTTPS_PORT}:127.0.0.1:${HTTPS_PORT}" "${pa[@]}" \
    "https://${BENCH_HOST}:${HTTPS_PORT}/bytes/64" >"$out"
}
validate(){
  local f=$1 label=$2 e n
  jq -e . "$f" >/dev/null || die "$label invalid JSON"
  e=$(jq -r '[.errorDistribution[]]|add//0' "$f")
  n=$(jq -r '[.statusCodeDistribution|to_entries[]|select((.key|startswith("2"))|not)|.value]|add//0' "$f")
  ((e==0 && n==0)) || die "$label errors=$e non2xx=$n"
}
median(){ sort -n | awk '{a[NR]=$1}END{if(!NR)exit 1;if(NR%2)print a[(NR+1)/2];else print(a[NR/2]+a[NR/2+1])/2}'; }
med(){ awk -F '\t' -v i="$2" -v p="$3" -v c="$4" 'NR>1&&$1==i&&$2==p{print $c}' "$1" | median; }
order(){ case $((($1-1)%6)) in 0) echo 'baseline old-pgo new-pgo';;1) echo 'new-pgo old-pgo baseline';;2) echo 'old-pgo baseline new-pgo';;3) echo 'baseline new-pgo old-pgo';;4) echo 'new-pgo baseline old-pgo';;5) echo 'old-pgo new-pgo baseline';;esac; }
rps_delta(){ awk -v b="$1" -v v="$2" 'BEGIN{printf "%.2f",(v-b)/b*100}'; }
lat_delta(){ awk -v b="$1" -v v="$2" 'BEGIN{printf "%.2f",(b-v)/b*100}'; }

cat >"${OUTPUT}/environment.txt" <<EOF
baseline=${BASE_DIGEST:-$BASE_ID}
old_pgo=${OLD_DIGEST:-$OLD_ID}
new_pgo=${NEW_DIGEST:-$NEW_ID}
oha=${OHA_DIGEST:-$OHA_ID}
sut_cpu=${SUT_CPU}
backend_cpu=${BACKEND_CPU}
client_cpu=${CLIENT_CPU}
cfs_quota=disabled
probe_runs=${PROBE_RUNS}
latency_runs=${LATENCY_RUNS}
load_factor=${LOAD_FACTOR}
cooldown_seconds=${COOLDOWN_SECONDS}
available_bytes_at_start=${AVAILABLE_BYTES}
EOF
lscpu >>"${OUTPUT}/environment.txt"
docker version >>"${OUTPUT}/environment.txt"

printf 'image\tprotocol\tround\trps\tp50_ms\tp95_ms\tp99_ms\n' >"${PROBES}"
log "1단계 probe ${PROBE_RUNS}회"
for round in $(seq 1 "${PROBE_RUNS}"); do
  read -r -a imgs <<<"$(order "$round")"
  ((round%2)) && prots=(h1 h2) || prots=(h2 h1)
  for img in "${imgs[@]}"; do
    start_sut "$img"
    for p in "${prots[@]}"; do
      log "probe $round/${PROBE_RUNS}: $img $p"
      w="${RAW}/probe-warm-${round}-${img}-${p}.json"; run_probe "$p" "${WARMUP_SECONDS}" "$w"; validate "$w" "warm"
      j="${RAW}/probe-${round}-${img}-${p}.json"; run_probe "$p" "${PROBE_SECONDS}" "$j"; validate "$j" "probe"
      jq -r --arg i "$img" --arg p "$p" --arg r "$round" '[$i,$p,$r,.summary.requestsPerSec,(.latencyPercentiles.p50*1000),(.latencyPercentiles.p95*1000),(.latencyPercentiles.p99*1000)]|@tsv' "$j" | tee -a "${PROBES}"
    done
    cleanup_sut
  done
done

printf 'image\tprotocol\trps_median\tp50_ms_median\tp95_ms_median\tp99_ms_median\n' >"${PROBE_SUMMARY}"
for img in baseline old-pgo new-pgo; do for p in h1 h2; do
  printf '%s\t%s\t%s\t%s\t%s\t%s\n' "$img" "$p" "$(med "${PROBES}" "$img" "$p" 4)" "$(med "${PROBES}" "$img" "$p" 5)" "$(med "${PROBES}" "$img" "$p" 6)" "$(med "${PROBES}" "$img" "$p" 7)" >>"${PROBE_SUMMARY}"
done; done

slow(){ for img in baseline old-pgo new-pgo; do med "${PROBES}" "$img" "$1" 4; done | sort -n | head -n1; }
TARGET_H1=$(awk -v r="$(slow h1)" -v f="${LOAD_FACTOR}" 'BEGIN{print int(r*f)}')
TARGET_H2=$(awk -v r="$(slow h2)" -v f="${LOAD_FACTOR}" 'BEGIN{print int(r*f)}')
log "고정 QPS H1=${TARGET_H1} H2=${TARGET_H2}"

printf 'image\tprotocol\trun\ttarget_qps\tp50_ms\tp95_ms\tp99_ms\trps\tsuccess_rate\n' >"${MEASUREMENTS}"
log "2단계 동일 QPS ${LATENCY_RUNS}회"
for run in $(seq 1 "${LATENCY_RUNS}"); do
  read -r -a imgs <<<"$(order "$run")"
  ((run%2)) && prots=(h1 h2) || prots=(h2 h1)
  for img in "${imgs[@]}"; do
    start_sut "$img"
    for p in "${prots[@]}"; do
      [[ "$p" == h1 ]] && q=${TARGET_H1} || q=${TARGET_H2}
      log "latency $run/${LATENCY_RUNS}: $img $p qps=$q"
      w="${RAW}/lat-warm-${run}-${img}-${p}.json"; run_fixed "$p" "$q" "${WARMUP_SECONDS}" "$w"; validate "$w" "lat-warm"
      j="${RAW}/lat-${run}-${img}-${p}.json"; run_fixed "$p" "$q" "${TEST_SECONDS}" "$j"; validate "$j" "lat"
      jq -r --arg i "$img" --arg p "$p" --arg r "$run" --arg q "$q" '[$i,$p,$r,$q,(.latencyPercentiles.p50*1000),(.latencyPercentiles.p95*1000),(.latencyPercentiles.p99*1000),.summary.requestsPerSec,.summary.successRate]|@tsv' "$j" | tee -a "${MEASUREMENTS}"
    done
    cleanup_sut
  done
done

{
  echo '================================================================'
  echo ' Pingora 3-way low-noise benchmark'
  echo '================================================================'
  echo
  echo "CPU: Pingora=${SUT_CPU}, backend=${BACKEND_CPU}, oha=${CLIENT_CPU}; CFS quota disabled"
  echo "baseline: ${BASE_DIGEST:-$BASE_ID}"
  echo "old-pgo:  ${OLD_DIGEST:-$OLD_ID}"
  echo "new-pgo:  ${NEW_DIGEST:-$NEW_ID}"
  echo
  echo "Maximum throughput median (${PROBE_RUNS} rounds)"
  printf '%-5s %-10s %12s %14s\n' Proto Image RPS vs_baseline
  for p in h1 h2; do
    b=$(med "${PROBES}" baseline "$p" 4)
    for img in baseline old-pgo new-pgo; do
      v=$(med "${PROBES}" "$img" "$p" 4)
      printf '%-5s %-10s %12.1f %13s%%\n' "${p^^}" "$img" "$v" "$(rps_delta "$b" "$v")"
    done
  done
  echo
  echo "Equal-load latency median (${LATENCY_RUNS} rounds; no latency correction)"
  printf '%-5s %-10s %9s %9s %9s %11s %14s\n' Proto Image p50_ms p95_ms p99_ms RPS p99_vs_base
  for p in h1 h2; do
    b=$(med "${MEASUREMENTS}" baseline "$p" 7)
    for img in baseline old-pgo new-pgo; do
      p50=$(med "${MEASUREMENTS}" "$img" "$p" 5); p95=$(med "${MEASUREMENTS}" "$img" "$p" 6); p99=$(med "${MEASUREMENTS}" "$img" "$p" 7); r=$(med "${MEASUREMENTS}" "$img" "$p" 8)
      printf '%-5s %-10s %9.3f %9.3f %9.3f %11.1f %13s%%\n' "${p^^}" "$img" "$p50" "$p95" "$p99" "$r" "$(lat_delta "$b" "$p99")"
    done
  done
  echo
  echo 'New PGO vs old PGO (positive = improvement)'
  for p in h1 h2; do
    or=$(med "${PROBES}" old-pgo "$p" 4); nr=$(med "${PROBES}" new-pgo "$p" 4)
    op=$(med "${MEASUREMENTS}" old-pgo "$p" 7); np=$(med "${MEASUREMENTS}" new-pgo "$p" 7)
    echo "  ${p^^} throughput: $(rps_delta "$or" "$nr")%"
    echo "  ${p^^} p99:       $(lat_delta "$op" "$np")%"
  done
  echo
  echo "Files: ${OUTPUT}"
} | tee "${SUMMARY}"

log "완료: ${SUMMARY}"
