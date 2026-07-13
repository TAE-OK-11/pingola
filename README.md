# Pingora

Cloudflare [Pingora](https://github.com/cloudflare/pingora)를 기반으로 AWS-LC,
HTTP/1.1, HTTP/2를 지원하는 JBS 리버스 프록시입니다. 기본 설정은 PiKKY 정적
사이트, Navidrome, Vaultwarden, CouchDB, AdGuard Home/DoH를 1 vCPU / 1 GB Linux
호스트에서 운영하도록 bounded cache·buffer·limiter를 사용합니다.

이 프로젝트의 이전 이름은 Pingola였습니다. Cargo package는 upstream `pingora`
crate와 충돌하지 않도록 `jbs-pingora`, 실행 binary와 제품명은 `pingora`입니다.
upstream dependency는 Rust 코드에서 `cloudflare_pingora` alias로 가져옵니다.

## 주요 기능과 한계

- AWS-LC를 사용하는 rustls와 다운스트림 TLS 1.3 전용 정책
- HTTP/1.1 및 HTTP/2, 기본 최대 32개 동시 H2 stream(설정으로 1~1024 override)
- IPv4/IPv6 listener와 IPv6 socket의 명시적 `IPV6_V6ONLY=true`
- Host allowlist, trusted proxy 기반 `X-Forwarded-For`, body 크기 제한
- 서비스·route별 rate limit 및 active request/H2 stream limit
- Navidrome audio 무압축 streaming, Vaultwarden hub, CouchDB replication, DoH 정책
- PiKKY 정적 파일 gzip/Brotli/Zstd 동적·사전 압축, bounded LRU cache
- AWS-LC TLS 파일 사전 검사와 UID/GID/mode/symlink 대상 진단
- Rust global allocator로 정적 링크된 Google TCMalloc(8 KiB logical page)
- UID/GID `10001:10001`, read-only root filesystem, 최소 capability

Pingora 0.8.1은 다운스트림 HTTP/3/QUIC server를 제공하지 않으므로 HTTP/3와
`Alt-Svc`는 지원하지 않습니다. gzip/Brotli/Zstd는 PiKKY 정적 파일에만 적용하며
proxy 응답과 audio stream에는 동적 압축을 하지 않습니다.

## 새 이름과 한 릴리스 호환성

| 항목 | 새 기본값 | 임시 호환값 |
|---|---|---|
| binary | `pingora` | 없음 |
| image | `ghcr.io/tae-ok-11/pingora` | `ghcr.io/tae-ok-11/pingola` 동일 manifest |
| Compose service/container | `pingora` | 없음 |
| config env | `PINGORA_CONFIG` | `PINGOLA_CONFIG` 경고 후 fallback |
| config | `/etc/pingora/pingora.yaml` | `/etc/pingola/pingola.yaml` 경고 후 fallback |
| working directory | `/tmp/pingora` | 없음 |
| health | `/pingora-health` | `/pingola-health` 경고 후 alias |

구 config/env/health alias는 한 릴리스 뒤 제거할 예정입니다. 새 이름을 먼저
검색하고 구 이름을 실제로 사용할 때 프로세스당 한 번만 deprecated 경고를 냅니다.
`/nginx-health`에는 항상 `404`를 반환합니다.

저장소 이름은 코드와 CI에서 `TAE-OK-11/pingora`를 목표로 준비되어 있습니다.
저장소 소유자가 마지막에 다음 명령을 실행해야 합니다.

```bash
gh repo rename pingora --repo TAE-OK-11/pingola --yes
git remote set-url origin https://github.com/TAE-OK-11/pingora.git
```

## IPv4와 IPv6

기본 [`config/pingora.yaml`](config/pingora.yaml)은 다음 네 listener를 동시에
사용합니다.

```yaml
server:
  http_listen: ["0.0.0.0:80", "[::]:80"]
  https_listen: ["0.0.0.0:443", "[::]:443"]
```

AAAA record가 있는데 IPv6 listener가 없으면 IPv4에서는 정상이어도 AAAA를 먼저
선택하는 휴대폰에서 접속이 실패할 수 있습니다. IPv6 listener는 커널의
`net.ipv6.bindv6only` 기본값에 기대지 않고 `IPV6_V6ONLY=true`를 설정하므로 같은
port의 IPv4 wildcard와 중복 bind하지 않습니다. 다음 명령은 모든 주소를 동시에
실제 bind하여 충돌 주소를 표시합니다.

```bash
pingora --config config/pingora.yaml --check-bind
```

## Docker 배포와 인증서 mount

`main` 검증을 통과한 image는 SBOM/provenance와 함께 GHCR에 게시됩니다.

```bash
docker pull ghcr.io/tae-ok-11/pingora:latest
docker compose up -d
docker compose ps
```

기본 Compose는 기존 `10.77.0.1` 및 localhost upstream 접근을 위해 host network를
사용합니다. 같은 호스트의 80/tcp와 443/tcp가 비어 있어야 합니다. Compose의
`stop_grace_period: 65s`는 기본 60초 graceful drain보다 길게 잡혀 Docker가 재생 중인
stream을 10초 기본 timeout으로 강제 종료하지 않게 하며, file descriptor limit은
32,768, process/thread 수는 256으로 명시해 자원 증가를 bounded 상태로 유지합니다.

Let's Encrypt의 `live/<domain>/*.pem`은 실제 파일이 아니라
`archive/<domain>/*N.pem`을 가리키는 symlink입니다. `live` directory나 PEM
symlink만 mount하면 container 안에 archive 대상이 없어 broken symlink가 됩니다.
따라서 기본 Compose는 전체 tree를 mount합니다.

```yaml
volumes:
  - /etc/letsencrypt:/etc/pingora/cert:ro
```

다른 저장소를 사용하면 host root를 바꾸고 `config/pingora.yaml`의 내부 경로도
그 mount 구조에 맞춥니다.

```bash
PINGORA_CERT_ROOT=/srv/my-certificate-store \
PINGORA_CONFIG_FILE=./config/pingora.yaml \
docker compose up -d
```

## 개인키 ACL과 Certbot 갱신

개인키를 world-readable로 변경하지 마십시오. Container UID 10001에만 directory
traverse와 파일 read ACL을 줍니다. 아래 예시는 한 lineage에만 적용합니다.

```bash
DOMAIN=tae00217.cloud
sudo apt-get install -y acl
sudo setfacl -m u:10001:--x /etc/letsencrypt /etc/letsencrypt/live \
  "/etc/letsencrypt/live/$DOMAIN" /etc/letsencrypt/archive \
  "/etc/letsencrypt/archive/$DOMAIN"
sudo setfacl -R -m u:10001:r-X "/etc/letsencrypt/archive/$DOMAIN"
sudo setfacl -m d:u:10001:r-X "/etc/letsencrypt/archive/$DOMAIN"
sudo getfacl "/etc/letsencrypt/archive/$DOMAIN"
```

특정 `privkey6.pem`에만 ACL을 주면 갱신 때 생성되는 `privkey7.pem`에는 권한이
없습니다. archive lineage의 default ACL이나 Certbot deploy hook이 필요합니다.
저장소의 hook은 새 archive target 권한을 적용하고 container UID로 인증서 open,
PEM parse, certificate/key 일치를 검사한 뒤에만 Pingora를 재시작합니다.

```bash
sudo install -m 0755 scripts/certbot-deploy-hook.sh \
  /etc/letsencrypt/renewal-hooks/deploy/pingora
sudo PINGORA_COMPOSE_FILE=/opt/pingora/docker-compose.yml \
  certbot renew --dry-run --run-deploy-hooks
```

수동 갱신 검증은 다음과 같습니다.

```bash
readlink -f /etc/letsencrypt/live/tae00217.cloud/privkey.pem
PINGORA_COMPOSE_FILE=/opt/pingora/docker-compose.yml \
  scripts/validate-certificates.sh
docker compose restart pingora
docker compose exec pingora pingora --healthcheck
```

`--check` 오류에는 입력 경로, 현재 process UID/GID, 대상 owner UID/GID, mode,
symlink 여부, 최종 target과 존재 여부가 포함됩니다. 개인키 내용은 출력하지 않습니다.

## 두 단계 사전 검사

`--check`는 첫 오류에서 중단하지 않고 가능한 오류를 모두 수집합니다.

1. YAML/schema 및 host/upstream 참조 검증
2. certificate/key 실제 open, PEM, key match, symlink target, static root 검증

각 upstream 주소도 이 단계에서 socket address로 해석합니다. 해석 실패는 upstream
이름과 설정 주소를 함께 출력하며 요청이 들어온 뒤 peer 생성에서 panic하지 않습니다.
해석 결과는 process 시작 때 준비되므로 DNS 이름의 주소가 바뀌면 Pingora를 재시작해야
합니다. 기본 운영 config처럼 고정 IP upstream을 권장합니다.

실제 port bind는 운영 listener를 순간 점유할 수 있으므로 `--check-bind`에서만
추가합니다. 정상 startup은 race를 줄이기 위해 bind 검사를 포함한 전체 preflight를
항상 먼저 실행합니다.

```bash
pingora --config config/pingora.yaml --check
pingora --config config/pingora.yaml --check-bind
docker compose run --rm --no-deps pingora --check
```

## Healthcheck

Docker HEALTHCHECK는 `127.0.0.1:80`에 의존하지 않습니다. 설정의 local Unix socket
(`/tmp/pingora/health.sock`)을 binary 자체가 검사하므로 HTTP listener가 없거나,
HTTPS-only 또는 IPv6-only여도 동작합니다.

- `/pingora-health`: 저오버헤드 204 및 `X-Proxy-Product: Pingora`
- `/pingora-live`: process/listener liveness 204
- `/pingora-ready`: startup preflight와 listener가 완료된 readiness 204
- `/pingora-health/details`: `server.health_details: true`일 때 JSON
- `/pingora-health/details?upstreams=1`: 각 upstream TCP connect 결과, 실패 시 503

```bash
pingora --config config/pingora.yaml --healthcheck
PINGORA_HEALTH_TARGET='unix:/tmp/pingora/health.sock' pingora --healthcheck
pingora --healthcheck tcp:'[::1]:8080'
```

실패 메시지는 실제 검사한 `unix:` 또는 `tcp:` target을 출력합니다.

## Timeout, retry, limiter 설정

Upstream `read_timeout_seconds`와 `write_timeout_seconds`를 생략하면 route 기본값을
사용합니다. 명시하면 route 기본값보다 크거나 작아도 정확한 override입니다.
Navidrome stream/CouchDB 3600초, Vaultwarden hub 86400초, Vaultwarden UI 300초,
DoH 30초가 생략 시 기본값입니다.

`server.max_retries`는 최초 시도 뒤의 추가 retry 횟수입니다. `0`, `1`, `2`는 각각
총 1, 2, 3번 connect 시도입니다. Retry는 response가 client로 전송되기 전의 body
없는 GET/HEAD와 transient connect 오류에만 허용합니다. POST/PUT/stream body,
certificate 오류, upstream HTTP 502/503 response는 자동 재전송하지 않습니다.

`route_limits`에서 기존 기본값을 선택적으로 override할 수 있습니다.

```yaml
route_limits:
  navidrome_stream:
    rate_per_second: 40
    burst: 15
    active_requests: 12
  doh:
    rate_per_second: 0   # rate limiter disabled
    active_requests: 0   # active request/H2 stream limiter disabled
```

음수, NaN, infinity 및 1,000,000 초과 값은 거부합니다. Limit 이름의 단위는 TCP
connection이 아니라 활성 HTTP request이며 HTTP/2에서는 stream입니다. 서비스별
zone은 서로 격리되고, 전체 IP limit이 필요하면 `server.global_active_requests`를
별도로 설정합니다. 설정 reload는 아직 지원하지 않으므로 변경 후 재시작해야 합니다.
Rate limiter의 client bucket은 최대 262,144개로 제한하며, 한도에 도달하면 기존
client는 계속 처리하고 새로운 client는 idle bucket 정리 전까지 fail-closed로 429를
받습니다. 따라서 고유 source IP flood에서도 limiter memory가 무제한 증가하지 않습니다.
모든 limiter를 끈 [`config/benchmark.yaml`](config/benchmark.yaml)은 localhost
benchmark 전용이며 운영에 사용하면 안 됩니다.

## TCMalloc 선택과 진단

배포 binary는 `tcmalloc-better` 0.1.19가 포함한 Google TCMalloc/Abseil 소스를
8 KiB logical page 설정으로 빌드하고 Rust global allocator로 명시적으로
등록합니다. 빌드 중 별도 Git 저장소나 tarball을 받지 않으며 `LD_PRELOAD`에도
의존하지 않습니다. 1 vCPU 환경에서 확인되지 않은 background thread, huge-page,
arena 튜닝은 image에 강제하지 않습니다.

```bash
pingora --allocator-info
PINGORA_ALLOCATOR_STATS=1 pingora --config config/pingora.yaml
```

출력이 `allocator=tcmalloc implementation=google-tcmalloc`인지 CI와 Docker runtime
test에서 검사합니다. `PINGORA_JEMALLOC_STATS`는 이전 배포와의 환경변수 호환을
위한 deprecated fallback이며 새 배포에서는 사용하지 마십시오. TCMalloc의
huge-page 관련 환경값은 운영 서버 측정 없이 고정하지 않습니다.

`server.health_details: true`와 진단 환경변수를 모두 명시한 경우에만 실행 중인
process의 allocator counter를 조회할 수 있습니다. 평상시 hot path에서는 수집하지
않습니다. 진단값에는 current allocated bytes, heap size, physical/virtual memory,
peak memory, realized fragmentation, per-CPU cache 활성 여부가 포함됩니다.

```bash
PINGORA_ALLOCATOR_STATS=1 pingora --config config/pingora.yaml
curl -H 'Host: health.invalid' \
  'http://127.0.0.1/pingora-health/details?allocator=1'
```

system allocator와 기존 jemalloc은 배포 기본값이 아니라 회귀 비교와 즉시 rollback
용 feature로만 유지합니다.

```bash
cargo build --release --no-default-features --features system-allocator
cargo build --release --no-default-features --features jemalloc

# 게시된 jemalloc/tcmalloc image를 0.5 CPU/1 GiB에서 5라운드 교대 비교
ALLOCATOR_BENCH_CPUS=0.5 ALLOCATOR_BENCH_MEMORY=1g \
  ALLOCATOR_BENCH_ROUNDS=5 bench/allocator_images.sh
```

## 빌드와 검증

Rust toolchain은 1.97.0이며 Cargo lockfile은 직접 의존성의 최신 호환 버전을
고정합니다. GitHub Actions는 RustSec audit를 image 게시 전 실행하고, Dependabot이
Cargo, Docker base image, Actions를 매주 확인합니다. Cloudflare Pingora 0.8.1이
직접 고정한 일부 transitive crate와 deprecated `serde_yaml`은 upstream 제약 때문에
별도로 남아 있으며 `cargo update --dry-run --verbose`로 추적합니다. Pingora 0.8.1의
`prometheus 0.13`이 취약한 `protobuf 2.28`을 가져오는 경로는 core API를 바꾸지 않는
로컬 최소 패치로 `prometheus 0.14`/`protobuf 3.7.2` 이상을 사용합니다. 패치 근거와
범위는 `vendor/pingora-core-0.8.1/README.pingora-patch.md`에 기록합니다.

```bash
cargo fmt --check
cargo test --all-targets --locked
cargo clippy --all-targets --locked -- -D warnings
tests/runtime_preflight.sh
tests/listeners_health.sh
tests/retries.sh
tests/limit_isolation.sh
tests/integration.sh
tests/http2_matrix.sh
tests/http2_nginx_repro.sh
tests/service_matrix.sh
PINGORA_TEST_IMAGE=ghcr.io/tae-ok-11/pingora:local tests/docker_runtime.sh
docker build --build-arg ALLOCATOR=tcmalloc \
  --build-arg RUST_TARGET_CPU=x86-64-v2 -t pingora:local .
```

`tests/http2_matrix.sh`는 HTTP/1.1/H2 fixed length, chunked, trailer, 204, HEAD,
upstream close/keepalive/early EOF, TLS 1.3 ALPN, H2 concurrency 1/8/32를 검사하고
body SHA-256과 raw curl/nghttp/h2load log를 `/tmp/pingora-h2-matrix`에 남깁니다.
`tests/http2_nginx_repro.sh`는 upstream으로 반드시
`tae00217/jbs-nginx:ultra-4.0` image를 사용합니다.

## Profiling, H2 tuning, NGINX 비교

비교 도구는 운영 container를 중지하거나 이름을 재사용하지 않고 18700번대 고포트와
고유 container 이름만 사용합니다. 종료와 Ctrl+C 때 자신이 만든 container와
backend만 정리합니다. 한 case가 실패해도 다음 case를 계속하고 실패를 0 RPS로
바꾸지 않습니다. raw curl/wrk/h2load, container log, CPU/RSS sample, image inspect,
`nginx -V`, `ldd`, 실제 `nginx -T`를 결과 directory에 보존합니다. 두 프록시에는
동일한 forwarded/security header, downstream HTTP/1.1 500-request keepalive,
upstream pool, timeout, nofile 32768을 적용합니다. 기본 0.5 CPU/1 GiB 제한은 시작
후 Docker inspect로 재확인하며, raw log 때문에 디스크가 가득 차지 않도록 시작 시
최소 1 GiB 여유 공간을 요구합니다. `summary.txt`와 `summary.tsv`에는 5라운드
paired 중앙값과 RPS/CPU 효율 기하평균이 생성됩니다.

```bash
# 빠른 도구/무결성 확인
BENCH_PROFILE=smoke BENCH_ROUNDS=1 bench/compare.sh

# Oracle 운영 호스트에서 실행하는 공식 5-round 전체 비교
sudo BENCH_PROFILE=full BENCH_ROUNDS=5 \
  BENCH_CPUS=0.5 BENCH_MEMORY=1g \
  PINGORA_IMAGE=ghcr.io/tae-ok-11/pingora:latest \
  NGINX_IMAGE=tae00217/jbs-nginx:ultra-4.0 \
  BENCH_OUTPUT=/var/tmp/pingora-bench-$(date -u +%Y%m%dT%H%M%SZ) \
  bench/compare.sh

# perf stat/record, flamegraph 입력, strace -c, allocator 전후 counter
sudo PROFILE_CPUS=0.5 PROFILE_MEMORY=1g PROFILE_DURATION_SECONDS=30 \
  bench/profile.sh

# max concurrent streams 32/64/128/256 교차 측정
sudo bench/h2_tuning.sh
```

기본값 32는 개발 호스트 교차 측정에서 128보다 평균 RPS는 낮았지만 p99와 peak
memory가 유의미하게 낮아 p99 우선 정책으로 선택했습니다. 운영 Oracle 서버에서는
위 스크립트로 다시 측정한 뒤 `server.http2_max_concurrent_streams`를 override하십시오.

전체 profile은 0B~100MiB, concurrency/H2 stream 1~128을 실행하므로 충분한 시간을
확보해야 합니다. Oracle 1 vCPU에서는 load generator·proxy·synthetic backend가
같은 CPU를 경쟁합니다. 결과는 절대 RPS만 보지 말고 오류/SHA-256, p99, CPU당
처리량, peak RSS 순서로 평가합니다. `bench/profile.sh`는 운영 binary가 stripped라
source symbol이 제한될 수 있어 `perf.data`와 `perf script` 입력을 모두 남깁니다.

공식 generic image는 `x86-64-v2`입니다. Oracle 전용 image를 만들 때는 서버에서
`lscpu`와 `rustc --print target-cpus`를 먼저 확인하고 실제 CPU와 일치하는 값만
`RUST_TARGET_CPU`로 전달하십시오. CPU를 추측해 `znver1` 등을 고정하지 않습니다.

## 배포와 rollback

기존 NGINX가 80/443을 점유한 상태에서는 `--check`까지만 먼저 수행합니다.
인증서·정적 파일·YAML을 모두 확인한 뒤 짧은 전환 구간에 기존 proxy를 중지하고
bind 검사와 기동을 수행합니다.

```bash
cd /opt/pingora
git pull --ff-only
docker compose pull pingora
docker compose run --rm --no-deps pingora --check
docker stop jbs-nginx
docker compose run --rm --no-deps pingora --check-bind
docker compose up -d pingora
docker compose exec pingora pingora --healthcheck
curl --fail --resolve music.tae00217.cloud:443:127.0.0.1 \
  https://music.tae00217.cloud/pingora-health
```

실패 시 정확한 rollback은 새 container를 내리고 기존 container를 다시 시작하는
것입니다.

```bash
cd /opt/pingora
docker compose down
docker start jbs-nginx
curl --fail http://127.0.0.1/nginx-health
```

## NGINX 대비 동작 차이

- 알 수 없는 Host는 비표준 444 대신 `421 Misdirected Request`로 거부합니다.
- HTTP/3는 지원하지 않습니다.
- Navidrome audio는 전체 buffering하거나 동적 압축하지 않습니다.
- DoH upstream의 기존 운영 동작을 맞추기 위해 기본 config의
  `verify_certificate: false`가 유지됩니다. 내부 CA를 배포할 수 있으면 반드시
  `true`로 전환하십시오.

## 라이선스

Apache-2.0. Vendored Pingora rustls adapter는 원본 Cloudflare 저작권과 라이선스를
보존합니다.
