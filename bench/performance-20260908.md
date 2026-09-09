# Pingola / Navidrome 성능 분석 및 빌드 예산

기준: Pingola `87f56cd`, GitHub Actions [34219013321](https://github.com/TAE-OK-11/pingola/actions/runs/34219013321).
요청에 따라 서버에서 빌드, 로컬 테스트, 부하 측정은 실행하지 않았다.

## 관측과 근본 원인

- 기존 workflow: 2026-09-08 11:07:18–15:08:11 UTC, 4시간 53초.
- verify: 5분 23초. image build/publish 단계: 3시간 55분 5초.
- PGO 도구 컴파일 4분 33초, Rust 계측 4분 57초, native 계측용
  Rust 빌드 4분 37초, 최종 PGO 빌드 5분 15초. 합계 약 19분 22초.
- upstream H3 학습이 긴 구간을 차지한다. 예를 들어 Rust 2회차는
  11:56:03–12:32:11(36분 8초), native BBR2 2회차는
  13:48:06–14:25:37(37분 31초). 학습 probe가 최대 8회 재시도하며
  연결이 닫힐 때 origin/target listener를 재시작한다. 이 구조가 학습 지연을
  늘릴 수 있다. 로그 시간만으로 실제 서비스의 QUIC 지연 원인까지 단정하지 않는다.
- Cargo registry/target은 BuildKit cache mount 안에만 저장돼 있었다.
  [Docker 문서](https://docs.docker.com/build/ci/github-actions/cache/)에 따르면
  이 mount는 기본적으로 GHA cache로 보존되지 않는다. 소스 변경 시 새 러너가
  컴파일 산출물을 다시 사용할 수 없는 구조였다.

## 실제 요청 경로

```text
클라이언트 → [경로에 따라 CDN] → Pingola TLS/H2 또는 QUIC/H3
           → 127.0.0.1:4533 HTTP/1 keepalive → Navidrome
```

- 운영 Pingola는 host network, upstream pool 128, worker 2, access_log=false.
  Navidrome은 이미 로컬 plaintext H1이며 내부 TLS/QUIC를 중복 수행하지 않는다.
- Navidrome API/cover는 origin 압축 협상을 전달한다. 음악 stream은 proxy에서
  재압축하지 않는다. 이미 적용된 최적화이므로 중복 변경하지 않았다.
- 당시 단일 수동 관측: Pingola CPU 0.12%, memory 4.445 MiB/192 MiB;
  Navidrome CPU 0.01%, memory 12.55 MiB. 순간값으로 피크 병목은 알 수 없다.
- Navidrome 운영 설정 GOMAXPROCS=1은 API/스캔/트랜스코딩 부하에서 별도
  병목 후보이나, 부하 증거 없이 CPU/메모리 설정을 변경하지 않았다.
- QUIC stateless retry는 신규 연결에 추가 왕복을 만들 수 있으나 admission
  보호를 유지했다. TCP/QUIC 연결 재사용과 신규 연결 지연은 구분해야 한다.

## 변경

1. 일반 push는 PGO를 끄되 **znver3, fat LTO, codegen-units=1, opt-level=3**,
   jemalloc/BoringSSL을 유지한다. 기존 README의 fat/Thin 비교 결과를 고려했다.
   full PGO는 수동 `build_profile=full-pgo`로 기존 학습을 실행할 수 있다.
2. registry와 실제 의존성 컴파일을 소스와 독립적인 Docker layer로 보존한다.
   dummy root의 fingerprint와 binary는 지워 실제 코드를 반드시 다시 컴파일한다.
   PGO 중간 산출물은 최종 build layer에 남기지 않는다.
3. upstream H3가 하나도 없는 구성에서는 downstream H3의 원본 wire header를
   변환·보관하지 않는다. 운영 Navidrome H1 경로에서 두 번째 header set이
   음악 stream 수명 동안 남는 것을 막는다. upstream H3가 있으면 기존 capture를 유지한다.
   H3 wire reconciliation에서 lowercase 이름의 Bytes를 공유하고 HeaderMap의
   정규화된 이름 slice로 조회한다. 기존의 헤더당 삽입/조회용 Vec 할당 및
   lowercase scratch allocation을 제거한다. 실제 이득의 크기는 미측정이다.
   중복 header, 필터로 바뀐 값, 삭제한 forwarded header 보존 규칙은 유지한다.
4. GitHub image-test에 API 512 B, cover 4 KiB, stream 64 KiB 직접/프록시
   H1 측정을 추가한다. 동시성 1/8/32, 3회 순서 교대, 2초 warmup/5초 측정.
   body SHA, HTTP/transport error를 확인하고 RPS와 p99 차이(µs)를 보관한다.
   오류 표본을 버리고 정상 결과처럼 요약하지 않는다.

## 검증 및 한계

로컬 실행 없이 GitHub에서 Rust 테스트/Clippy, allocator fallback,
H1/H2/H3 통합, image runtime 검증, synthetic overhead 측정을 수행한다.
CI 결과와 실제 소요 시간은 아래 최종 기록에 정리했다.

일반 publish job 예산은 25분이고 verify를 포함해 30분 이내가 목표다.
Timeout을 줄인 것 자체는 성공 증거가 아니다. 첫 콜드 캐시와 다음 캐시 적중
빌드는 구분해 확인해야 한다. full PGO는 30분 목표에 포함하지 않는다.

루프백 H1 benchmark는 인터넷 ping, CDN, TLS handshake, 실제 DB/음원 처리,
QUIC 성능을 측정하지 않는다. shared runner의 포화 p99 차이는 대기열/CPU 경쟁도
포함하며 순수 처리 비용이 아니다. HTTP 프록시는 추가 socket I/O와 스케줄링을
수행하므로 오버헤드 0 또는 특정 지연 감소율을 보장할 수 없다.
운영 컨테이너는 기존 digest에 고정돼 있으며 이 작업은 소스/CI 게시만 수행한다.

## GitHub 최종 검증 — 2026-09-09

검증 소스: `793f32344e59b1e1a10f2cc53300f556d9c4ff9a`.
[Actions 34315663127](https://github.com/TAE-OK-11/pingola/actions/runs/34315663127): 모든 job 성공.

| 단계 | 실제 시간 |
| --- | ---: |
| 전체 workflow (05:37:59–05:57:41 UTC) | 19분 42초 |
| verify | 5분 16초 |
| production image build + publish 단계 | 6분 35초 |
| publish job 전체 | 7분 2초 |
| portable image build | 7분 5초 |
| 직접 연결/프록시 benchmark 단계 | 6분 49초 |

기존 전체 4시간 53초 대비 일반 workflow가 약 91.8% 단축됐다.
full PGO 학습을 생략한 서로 다른 빌드 정책의 비교이며, PGO와 동일한
실행 성능이라는 의미가 아니다. fat LTO/Zen 3는 유지했다.
새 의존성 layer의 release 컴파일은 2분 26초, 실제 소스 컴파일은 1분 14초였다.
첫 실행에서 30분 목표를 달성했다. 후속 캐시 적중 빌드 시간은 아직 측정하지 않았다.

Rust 테스트/Clippy, allocator fallback, retry/limit 격리, H1/H2/H3 통합,
H3 strict/preferred fallback, image runtime 검사 모두 통과했다.
세 benchmark의 54개 측정 row는 모두 PASS이며 HTTP/transport error는 0이었다.

게시 이미지 (두 저장소의 동일 manifest digest):
`ghcr.io/tae-ok-11/pingora@sha256:c6ecc42bc66ce56b13a645d99d2b538a916d726e6948b06e8361ae19034c89df`
및 `ghcr.io/tae-ok-11/pingola`의 같은 digest.
운영 서버에 이 이미지를 적용하거나 재시작하지 않았다.

### 직접 backend 대비 proxy p99

단위 µs. 같은 runner에서 3회 측정한 각 target p99의 중앙값 차이이며,
개별 요청별 추가 지연의 p99는 아니다. H1 평문, rate/active limit off,
proxy security headers on, synthetic backend와 부하 생성기가 CPU를 공유한다.

| 요청 | 동시성 | Direct p99 | Proxy p99 | 차이 |
| --- | ---: | ---: | ---: | ---: |
| API 512 B | 1 | 63 | 170 | 107 |
| API 512 B | 8 | 162 | 394 | 232 |
| API 512 B | 32 | 580 | 1624 | 1044 |
| Cover 4 KiB | 1 | 70 | 176 | 106 |
| Cover 4 KiB | 8 | 170 | 440 | 270 |
| Cover 4 KiB | 32 | 618 | 1693 | 1075 |
| Audio 64 KiB | 1 | 103 | 247 | 144 |
| Audio 64 KiB | 8 | 395 | 820 | 425 |
| Audio 64 KiB | 32 | 1430 | 2983 | 1553 |

동시성 1의 추가 p99는 약 0.106–0.144 ms, 동시성 32에서는 1.044–1.553 ms였다.
포화 처리량은 direct 대비 API/cover 약 43–49%, stream 약 24–28% 낮았다
(동시성 8/32). 즉 추가 비용은 남아 있다. 단순 응답 backend는 극단적으로
가벼우므로 이를 실제 Navidrome 처리량 감소율로 해석하면 안 된다.
현재 자료는 변경 전후 성능 비교가 아니며 HTTP/3 수정의 개선율도 증명하지 않는다.

전체 raw/환경/이미지 provenance는 위 run의
`proxy-overhead-793f32344e59b1e1a10f2cc53300f556d9c4ff9a` artifact에 14일간 보관한다.
