# jemalloc vs TCMalloc: 0.5 CPU / 1 GiB

실행 시각은 2026-07-13 UTC이며 Pingora proxy container에만 Docker
`--cpus=0.5 --memory=1g --memory-swap=1g`를 적용했다. Docker inspect에서 실제
값 `NanoCpus=500000000`, `Memory=1073741824`, `MemorySwap=1073741824`를 확인한 뒤
부하를 시작했다.

## 비교 대상

- jemalloc: `ghcr.io/tae-ok-11/pingora@sha256:78ccd4006270d6ccc98be36a6d912a7bd0a16108e87f6aa4af56a57c7da8cff5`
- TCMalloc: `ghcr.io/tae-ok-11/pingora@sha256:46244afee72d96cc2aa465416358febdfbbeadd086a875a1cb032ce163c35b44`
- backend: `tae00217/jbs-nginx:ultra-4.0` image ID
  `sha256:f3c1b71ee1f9e2450f54892823b805572a742c25f79dae9319f325154dbe9da1`
- host: AMD EPYC 7763, 2 vCPU KVM host. Load generator와 backend는 host에서
  실행하고 비교 대상 proxy만 0.5 CPU로 제한했다.

두 image는 같은 Pingora 설정, TLS certificate, localhost backend, port,
upstream keepalive, timeout, disabled access log/limiter를 사용했다. 64 B와 4096 B,
동시성 1/8/32, HTTP/1.1 keepalive, H2 single connection, H2 multi connection을
각각 5라운드 실행했다. 홀수 라운드는 jemalloc부터, 짝수 라운드는 TCMalloc부터
실행했다. 각 측정은 1초 warmup과 3초 main duration을 사용했다.

## 결과

각 workload에서 allocator별 5라운드 중앙값을 구한 뒤 비교했다. RPS와 CPU 효율은
양수가 TCMalloc 우세, p99와 RSS는 음수가 TCMalloc 우세다.

| 구간 | case | RPS | p99 | RPS/CPU | peak RSS |
|---|---:|---:|---:|---:|---:|
| 전체 | 18 | +4.42% | -8.06% | +5.52% | -16.17% |
| HTTP/1.1 keepalive | 6 | +6.22% | -8.65% | +7.21% | -22.06% |
| H2 single connection | 6 | +4.10% | -3.44% | +5.44% | -15.83% |
| H2 multi connection | 6 | +2.95% | -9.27% | +3.93% | -15.90% |
| concurrency 1 | 6 | +3.18% | -5.31% | +5.85% | -18.66% |
| concurrency 8 | 6 | +3.69% | -21.93% | +4.15% | -14.82% |
| concurrency 32 | 6 | +6.41% | -3.75% | +6.58% | -16.78% |

- TCMalloc은 RPS 18 case 중 16개, p99 18개 중 16개, CPU 효율 18개 중
  17개, peak RSS 18개 전부에서 우세했다.
- 절대 최대 peak RSS는 jemalloc 44,988 KiB, TCMalloc 39,496 KiB로
  TCMalloc이 5,492 KiB 낮았다.
- 전체 180개 측정 row가 PASS였고 wrk/h2load failed, errored, timeout은 0이었다.
- 각 case 시작 전에 backend 원본과 proxy 응답 body SHA-256을 비교했으며 mismatch는
  없었다.
- 각 라운드의 TCMalloc RPS 기하평균 변화는 각각 +3.82%, +4.73%, +2.10%,
  +2.11%, +6.57%로 모든 라운드에서 양수였다.

TCMalloc이 밀린 case는 HTTP/1.1 4096 B concurrency 1의 RPS -1.54%와 p99
+16.97%, H2 multi 64 B concurrency 8의 RPS -0.19%, H2 single 64 B
concurrency 1의 p99 +4.65%였다. 나머지 주요 지표와 메모리 결과를 합치면 이
0.5-core workload에서는 TCMalloc을 production 기본값으로 유지하는 판단을
지지한다.

Timing-based h2load가 duration 경계의 in-flight request를 status `0`으로 남긴
항목은 jemalloc 10개, TCMalloc 27개였다. h2load summary에서는 모두 failed,
errored, timeout 0이었고 이는 완료 HTTP response가 아니므로 latency 집계에서
제외했다. 유효 HTTP status만으로 다시 계산해도 위 전체 p99 결과는 -8.06%로
동일했다.

## 재현

```bash
ALLOCATOR_BENCH_CPUS=0.5 \
ALLOCATOR_BENCH_MEMORY=1g \
ALLOCATOR_BENCH_ROUNDS=5 \
ALLOCATOR_BENCH_DURATION=3s \
ALLOCATOR_BENCH_WARMUP=1s \
bench/allocator_images.sh
```

Raw 결과는 실행 host의
`bench/results/allocator-images-20260713T151305Z/`에 보존했다. 이 결과는 같은
host에서 상대 비교한 값이며 Oracle 1 vCPU 서버의 절대 처리량을 대신하지 않는다.
