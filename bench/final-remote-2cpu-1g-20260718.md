# Pingora 대 JBS NGINX 원격 최종 검증 — 2026-07-18

## 1. 비교 대상과 실행 조건

- source revision: `fca076d1bca251ccc292d8ac741705327ffc96c3`
- Actions run: <https://github.com/TAE-OK-11/pingola/actions/runs/29631630199>
- Pingora: `ghcr.io/tae-ok-11/pingora@sha256:d09f32e53976f153b36e44a1f3c6414491064398c09e9093695ef67555f2af9d`
- NGINX: `tae00217/jbs-nginx@sha256:f3c1b71ee1f9e2450f54892823b805572a742c25f79dae9319f325154dbe9da1`
- host: Oracle KVM, AMD EPYC 7551, 2 vCPU, 954 MiB physical RAM, swap 없음
- topology: 물리 1 core의 SMT 2 thread
- proxy 제한: 양쪽 모두 `--cpus=2 --memory=1g --memory-swap=1g`, worker 2
- 부하 경로: 동일 host load generator → proxy → 동일 localhost Rust HTTP/1 backend
- profile: H1 keep-alive, H2 single connection, H2 multi connection
- payload/concurrency: 64/4096 B, concurrency 1/8/32
- 각 case: 2초 warmup + 10초 측정, 3회 반복
- 순서: NGINX→Pingora, Pingora→NGINX, NGINX→Pingora
- access log/compression/cache/rate limit/active-request limit: 양쪽 모두 off
- TLS, 보안 header, upstream keepalive, timeout과 Docker network mode: 동일

컨테이너에 2 vCPU 전체를 허용했지만 load generator와 backend도 같은 물리 core를
공유한다. 따라서 절대 RPS에는 host 경쟁이 포함된다. 순서 교대와 동일 조건의 상대
비교를 우선해서 해석한다.

## 2. 재현된 문제와 근본 원인

1. H1 upstream request clone이 의미 없는 원본 header-name 대소문자 map까지 매
   요청 복제했다. semantic `HeaderMap`만 복제하고 raw request target fallback은
   별도로 보존했다. 0.5 CPU/1 GiB, 3회 개발 A/B에서 RPS +1.71%, RPS/CPU
   +2.26%, p99 -3.54%를 확인한 뒤 채택했다.
2. 일반 사용자로 벤치를 실행하면 임시 인증서를 `root:10001`로 `chown`하는 단계가
   실패했다. 파일 owner의 group과 컨테이너 supplementary group을 맞추고 mode
   0640을 유지하도록 수정했다. world-readable private key는 사용하지 않는다.
3. Ubuntu nghttp2 1.59의 `h2load`에는 `--sni`가 없었다. URL authority와
   `--connect-to`를 사용해 SNI와 접속 주소를 분리했다.
4. 새 h2load 방식에 `Host:`를 추가하면 URL의 `:authority`와 충돌해 NGINX가
   400을 반환했다. 한 요청을 Host 유무로 직접 재현했고, URL authority만 사용하는
   RFC 정상 요청으로 수정했다.
5. non-root 사용자의 `kill -0`은 UID 0/10001 컨테이너 프로세스에 EPERM을
   반환해 CPU/RSS resource 파일이 비었다. `/proc/PID` lifetime으로 sampler를
   제어하도록 수정했다.
6. resource 표본이 없는 실패 run에서 CPU efficiency 집계가 빈 배열을 0으로
   나눴다. 값이 없는 지표만 생략하고 나머지 실패 결과는 끝까지 보존한다.
7. Docker builder가 Debian image에서 `curl | rustup`으로 toolchain을 설치했다.
   공식 `rust:1.97.1-slim-trixie` builder로 변경해 네트워크 설치 단계를 없애고
   `/usr/local/cargo` cache를 사용한다.
8. lockfile이 최신 호환 release보다 뒤에 있었다. AWS-LC Rust 1.17.3,
   aws-lc-sys 0.43.0, Tokio 1.53.0, tokio-macros 2.7.1,
   portable-atomic 1.14.0으로 갱신했다.

## 3. 재현하지 못한 문제

이전의 정상 fixed/chunked 응답에서 발생했다는 HTTP/2 `CANCEL`은 재현되지
않았다. fixed, chunked, trailer, 204, HEAD, upstream close/keepalive,
concurrency 1/8/32와 TLS 1.3 ALPN matrix가 모두 통과했다. 최종 벤치에서도
108개 row의 transport/HTTP error와 6회 × 480 H2 stability probe error가 모두
0이었다. 증명되지 않은 hop-by-hop header 원인으로 단정하지 않는다.

timed `wrk` 종료 시 아직 열려 있던 downstream H1 연결이 닫히며 Pingora log에
`Connection reset by peer`가 두 번 기록됐다. 완료 요청 수, tool error, body
검증에는 영향이 없었으며 H2 stream reset은 아니다.

## 4. 변경 파일

- `.github/workflows/ci.yml`
- `Cargo.lock`
- `Dockerfile`
- `bench/compare.sh`
- `bench/h1-fastpath-0.5cpu-1g-20260716.md`
- `bench/pgo_compare_three.sh`
- `bench/summarize_compare.py`
- `tests/integration.sh`
- `vendor/pingora-proxy-0.8.1/src/proxy_h1.rs`

커밋은 `201dcd6`, `8d48abc`, `99813fa`, `62dd6f0`, `fca076d`이다.

## 5. 추가·강화한 테스트

- optimized H1 clone에서 percent-encoded raw target과 header value 보존
- PGO 비교 image의 revision/digest/allocator/TLS provenance
- H1/H2 64/4096 B body SHA-256
- Docker CPU/memory limit과 affinity 확인
- non-root benchmark private-key access
- h2load `--connect-to` 기반 SNI/authority 호환
- non-root cgroup CPU/RSS sampler
- 빈 resource metric 집계 방어
- 양쪽 worker 수를 `BENCH_WORKERS`로 동일하게 지정

## 6. 검증 결과

- `cargo fmt --check`: PASS
- `cargo test --all-targets --locked`: 50 PASS
- `cargo clippy --all-targets --locked -- -D warnings`: PASS
- RustSec audit: PASS
- BoringSSL dependency absence / AWS-LC-only graph: PASS
- runtime preflight, IPv4/IPv6/dual-stack, retry 0/1/2: PASS
- limiter isolation, static global limit, service matrix: PASS
- HTTP/2 correctness matrix: PASS
- generic/znver1 fat-LTO Docker clean build: PASS
- UID 10001, read-only filesystem, HTTP-only/HTTPS-only/IPv6-only health: PASS
- 원격 znver1 PGO runtime suite: PASS
- 최종 benchmark: 108/108 PASS, result error 0, stability error 0

## 7. 성능 결과

부호는 Pingora가 NGINX 대비 변한 값이다. RPS/CPU efficiency는 양수가 좋고,
p99는 음수가 좋다.

| 범위 | RPS | p99 | RPS/CPU | peak RSS |
|---|---:|---:|---:|---:|
| 전체 18 paired case | +11.66% | -1.06% | +3.36% | +15.30% |
| H1 keep-alive | -11.52% | -0.23% | -18.48% | +6.35% |
| H2 multi | +19.71% | -1.89% | +15.24% | +46.60% |
| H2 single | +31.45% | -5.37% | +17.52% | +11.43% |

전체 case의 절대 중앙값은 다음과 같다.

| proxy | RPS | p99 | CPU | per-case peak RSS |
|---|---:|---:|---:|---:|
| NGINX | 1,806.40 | 76.482 ms | 64.07% | 26.18 MiB |
| Pingora | 2,146.52 | 75.946 ms | 70.46% | 33.15 MiB |

3개 round의 전체 RPS 기하평균은 NGINX 1,631.29/1,642.34/1,672.79,
Pingora 1,827.66/1,826.27/1,873.45였다. 순서가 바뀐 round 2에서도 상대 추세가
유지됐다.

Pingora는 전체와 H2에서 이겼지만 H1 RPS와 H1 CPU 효율에서는 아직 NGINX를
이기지 못했다. 이 결과를 숨기거나 H1을 제외해 성능 승리로 왜곡하지 않는다.
추가 H1 변경은 perf profile과 동일 image A/B로 개선이 증명될 때만 채택해야 한다.

## 8. 메모리·크기 변화

- 최종 Pingora image: 36,251,634 B (34.57 MiB)
- 비교 NGINX image: 38,146,728 B (36.38 MiB)
- Pingora image는 NGINX보다 4.97% 작다.
- Pingora binary: 11,616,096 B (11.08 MiB)
- NGINX binary: 3,265,560 B (3.11 MiB)
- Pingora binary는 정적 AWS-LC/TCMalloc/Rust 코드 때문에 3.56배 크다.
- 측정 중 최대 RSS: Pingora 47.04 MiB, NGINX 37.64 MiB
- 직전 PGO image 36,260,741 B 대비 최종 image는 9,107 B 작다.

1 GiB 제한 안에서 메모리는 bounded였고 OOM은 없었다. Pingora의 H2 처리량과
tail latency 개선 대가로 peak RSS가 증가했다.

## 9. 호환성·보안 영향

- 실행 allocator는 진단 API에서 Google TCMalloc로 확인했다.
- TLS dependency는 AWS-LC/rustls만 선택됐고 BoringSSL은 graph에 없다.
- TLS 1.3 정책은 유지했다.
- private key를 로그나 artifact에 포함하지 않았다.
- raw archive에서도 `key.pem`, `cert.pem`, response body와 backend binary를
  제외했다.
- H1 header name은 RFC상 case-insensitive이므로 upstream에서 원래 spelling을
  보존하지 않아도 의미가 바뀌지 않는다. header value와 raw target은 보존한다.
- 기존 system allocator와 jemalloc feature는 rollback compile test를 유지한다.

## 10. raw 결과와 남은 위험

재현 archive:

- `bench/artifacts/pingora-nginx-fca076d-2cpu-1g-3r-raw.tgz`
- SHA-256: `caa2aed4893d3d126f66d0218150cad67f08c2bd218dd371a67d4973dcd8e40b`
- 547 files: wrk/h2load output, request latency log, cgroup resource samples,
  container log, image inspect/history, NGINX build/config dump, summary

남은 위험은 다음과 같다.

1. H1 keep-alive RPS -11.52%, CPU efficiency -18.48%가 가장 큰 미해결 성능 gap이다.
2. 비교 host는 2 vCPU지만 실제 물리 1 core/SMT 2 thread이며 부하 생성기와
   backend가 이를 공유했다.
3. host 물리 RAM은 954 MiB라 Docker의 1 GiB limit보다 작고 swap이 없다.
4. 최종 성능 profile은 64/4096 B proxy hot path다. 대형 stream, 신규 TLS
   handshake와 실제 혼합 서비스는 기능 matrix로 검증했지만 이 최종 수치에는 없다.
5. HTTP/3는 현재 지원하지 않는다.
6. 기본 DoH 예제의 upstream certificate verification off는 운영 호환용이다.
   내부 CA를 배포할 수 있으면 반드시 켜야 한다.
7. GitHub repository 자체 이름은 아직 `pingola`다. rename 권한이 있는 계정에서
   `gh repo rename pingora --repo TAE-OK-11/pingola --yes`를 실행한 뒤 OCI
   source/url을 `/pingora`로 동시에 바꿔야 한다.

## 11. 운영 적용과 rollback

AMD Zen 1 이상 서버에서 이번에 검증한 exact digest를 적용한다.

```bash
cd /opt/pingora
git pull --ff-only origin main
export PINGORA_IMAGE=ghcr.io/tae-ok-11/pingora@sha256:d09f32e53976f153b36e44a1f3c6414491064398c09e9093695ef67555f2af9d
export PINGORA_CERT_ROOT=/etc/letsencrypt
docker compose pull pingora
docker compose run --rm --no-deps pingora --check
docker stop jbs-nginx
docker compose run --rm --no-deps pingora --check-bind
docker compose up -d pingora
docker compose exec pingora pingora --healthcheck
curl --fail --resolve music.tae00217.cloud:443:127.0.0.1 \
  https://music.tae00217.cloud/pingora-health
```

실패 시 즉시 rollback한다.

```bash
cd /opt/pingora
docker compose stop pingora
docker start jbs-nginx
curl --fail http://127.0.0.1/nginx-health
```
