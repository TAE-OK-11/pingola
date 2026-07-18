# HTTP/1 bodyless fast path: 0.5 CPU / 1 GiB

2026-07-16 UTC에 동일한 source revision에서 만든 fat-LTO/tcmalloc native
binary 두 개를 비교했다. 비교 대상 proxy container에만
`--cpus=0.5 --memory=1g --memory-swap=1g`를 적용했고, localhost의 동일 Rust
backend와 동일 설정을 사용했다. 각 case는 순서를 교대해 3회 실행했으며 2초
warmup 뒤 10초 측정했다.

- baseline SHA-256:
  `ad15eb82ce8a4d169a0ab72cdaedfefd5fbbe555b69dcc6cbd6f5f22ef6fb165`
- fast-path SHA-256:
  `6f163fc5f26206abe032980413c6775ad56b2dd1c56841ef86b514aeaed97c46`
- workload: HTTP/1.1 keep-alive, 64 B/4096 B, concurrency 1/8/32
- host: AMD EPYC 7763 KVM, 2 vCPU

`allocator_images.sh`의 두 image slot 이름은 역사적인 이유로 `jemalloc`과
`tcmalloc`이지만, 이 비교에서는 두 image 모두 TCMalloc을 사용한다. 첫 slot이
baseline이고 두 번째 slot이 fast path다.

## 결과

6개 case의 baseline 대비 fast path 집계다. RPS와 CPU 효율은 양수가 개선,
p99와 RSS는 음수가 개선이다.

| 항목 | 변화 |
|---|---:|
| RPS 기하평균 | +4.65% |
| p99 중앙값 | -1.50% |
| RPS/CPU 기하평균 | +2.54% |
| peak RSS 중앙값 | +2.07% |
| 실패 row / body mismatch | 0 / 0 |

짧은 3초 측정도 별도로 실행했으며 RPS +6.33%, RPS/CPU +2.68%였지만 p99가
6.95% 나빠졌다. 긴 측정을 채택 판단의 기준으로 삼았다. host에는 load
generator와 backend도 함께 실행되므로 개별 case, 특히 concurrency 1 결과는
노이즈가 크다. 최종 성능 결론은 GitHub Actions가 만든 PGO image와 고정 NGINX
digest를 다시 비교한 결과로만 내린다.

## 채택 및 폐기

채택한 변경은 cache/upgrade/request body가 없는 opt-in HTTP/1 GET/HEAD에서
per-request 양방향 channel과 사용하지 않는 cache/range state를 만들지 않는
경로다. downstream disconnect 감지, response/body filter, framing, trailer와
upstream 오류 전달은 유지한다. 스트리밍·duplex route인 Navidrome stream,
Vaultwarden notification hub, CouchDB에는 적용하지 않는다.

다음 실험은 결과가 나빠져 모두 되돌렸다.

| 폐기한 실험 | RPS | p99 | RPS/CPU | peak RSS |
|---|---:|---:|---:|---:|
| passive upstream idle pool | -2.53% | -6.75% | -1.57% | +2.85% |
| hop-header 일괄 scan | -7.03% | -6.22% | -4.57% | +1.97% |

## 검증

- `cargo fmt --check`
- `cargo clippy --all-targets --locked -- -D warnings`
- `cargo test --all-targets --locked`: 50 passed
- runtime preflight, IPv4/IPv6/dual-stack, health, retry, limiter isolation
- Navidrome/Vaultwarden/CouchDB/DoH service matrix
- Vaultwarden/CouchDB의 HTTP/1 및 HTTP/2 gzip 회귀 검사
- HTTP/1 및 HTTP/2 fixed/chunked/trailer/204/HEAD/close/keepalive/early-EOF matrix
- nghttp, h2load concurrency 1/8/32, TLS 1.3 ALPN

로컬 raw 결과는
`bench/results/h1-bodyless-fast-v2-long-ab-0.5cpu-1g-3r/`에 보존된다.

재현 profile은 다음과 같다.

```bash
ALLOCATOR_BENCH_PROFILE=h1 \
ALLOCATOR_BENCH_CPUS=0.5 \
ALLOCATOR_BENCH_MEMORY=1g \
ALLOCATOR_BENCH_ROUNDS=3 \
ALLOCATOR_BENCH_WARMUP=2s \
ALLOCATOR_BENCH_DURATION=10s \
bench/allocator_images.sh
```

## 후속 최적화: upstream request case map 제거

2026-07-18 UTC에 bodyless fast path를 baseline으로 두고, HTTP/1 upstream
request를 만들 때 의미 없는 원본 header-name 대소문자 map을 복제하지 않는
후속 변경을 같은 조건으로 3회 A/B 측정했다. HTTP 의미와 header value는
그대로 유지하며 raw request target fallback도 보존한다.

| 항목 | baseline 대비 변화 |
|---|---:|
| RPS 기하평균 | +1.71% |
| p99 중앙값 | -3.54% |
| RPS/CPU 기하평균 | +2.26% |
| peak RSS 중앙값 | +4.55% |
| 실패 row / body mismatch | 0 / 0 |

절대 RSS는 약 25 MiB 범위였고 증가량은 bounded 상태였다. 64 B 및 4096 B,
concurrency 1/8/32의 총 36개 측정 row가 모두 성공했다. 유효한 percent-encoded
raw target과 혼합 대소문자 header value가 upstream에 그대로 도달하는 통합
회귀 검사도 추가했다. raw 결과는
`bench/results/h1-no-case-ab-0.5cpu-1g-3r/`에 보존된다. 이 수치는 변경 채택용
개발 A/B이며 최종 NGINX 비교 결론에는 사용하지 않는다.
