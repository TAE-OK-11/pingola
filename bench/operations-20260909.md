# 운영 Pingora 점검 — 2026-09-09

서버 부하 테스트/로컬 빌드 없이 Docker 로그, cgroup, 리스너와 운영 설정을 확인했다.
원본 요청 쿼리·인증값은 이 보고서에 포함하지 않는다.

## 관측

- 서버는 1 vCPU AMD EPYC Genoa, 메모리 951 MiB. Pingora CPU 순간값 0.11%,
  Docker working set 4.85 MiB. cgroup 메모리 현재 약 14.4 MiB, peak 22.8 MiB,
  swap 약 8.8 MiB. memory max/oom/oom_kill, CPU throttling은 모두 0.
  working set만으로 전체 사용량을 판단할 수 없다.
- Navidrome cgroup peak 약 351 MiB, swap 약 50 MiB, Docker hard memory limit 없음.
  GOMEMLIMIT=192MiB는 Go runtime 목표이며 Rust worker/FFmpeg까지 제한하지 않는다.
  지금 과다 사용/OOM의 증거는 없으므로 Navidrome 제한을 임의로 낮추지 않았다.
- host swap 약 1 GiB, vmstat 관측 구간에 swap-in이 있었으나 CPU idle 91–99%,
  IO wait 0–1%. 이 사실만으로 Pingora가 서버 전체 병목이라고 단정하지 않는다.
- 운영 Pingora revision은 `96939f5`, 기존 digest에 고정돼 앞선 최적화가 미적용.
- 08:46–09:22 UTC에 Navidrome `ConnectRefused` 로그 182줄. 요청 수가 아니라
  retry/최종/중복 로그를 포함한 수치다. 이후 새 Navidrome은 healthy이고 4533이
  listen 중이다. 당시 backend 중단 이력이며 지속적인 proxy CPU 병목 증거가 아니다.
- 11:02–11:04 커버 요청에 downstream H2 CANCEL이 발생했다. 일반 오류로
  분류하여 이미 취소된 스트림에 400을 쓰려다 추가 write 오류가 생겼다.
- 같은 시간대 Navidrome 자체가 커버 응답 429를 기록했다. proxy rate limit을
  풀어도 origin의 429는 해결되지 않는다. 원인 확정 없이 제한을 해제하지 않았다.
- error/retry request summary에 Subsonic 인증 query가 노출된다.
- 실제 Navidrome H2C listener는 overlay :50051에 있지만 운영 Pingora에는
  `navidrome_grpc` upstream이 빠져 gRPC를 일반 H1 origin으로 보낼 수 있었다.

## 수정 및 배포 계획

- 정확한 downstream H2 CANCEL만 정상 취소로 분류한다. 추가 400/write와
  retry/error log를 생략한다. upstream reset, protocol error, timeout은 유지한다.
  pinned H2 라이브러리가 만드는 CANCEL context를 실제 H2 reset CI 테스트로 고정한다.
- Gateway request_summary를 method/path만 출력하도록 바꿔 query와 auth header가
  정상적인 framework retry/failure summary에 기록되지 않도록 한다.
- `config/local-origin.yaml`: 실제 local H1 경로 유지, 전용 plaintext gRPC upstream
  추가, Navidrome 공통 3600초 timeout override 제거(API/cover 60초, stream 3600초),
  local connect timeout 10→2초. 기존 admission/rate/stream 제한은 유지한다.
- 운영 Compose의 Pingora memory+swap 예산을 memory와 같은 192MiB로 맞춰
  proxy swap을 금지한다. host 전체 swappiness는 변경하지 않는다. 현재 사용량은
  제한보다 충분히 작지만 심한 부하에서는 swap 대신 cgroup OOM 위험이 있으므로
  memory.events/peak를 계속 관측해야 한다.
- GitHub 검증/이미지 게시 후 immutable digest로 Pingora만 교체한다.
  기존 Compose/config를 백업하고 Docker health/readback으로 적용을 확인한다.
  Navidrome이나 다른 컨테이너는 재시작하지 않는다.

검증과 배포 결과는 아래 최종 기록에 정리했다.

## 사용자 추가 요청에 따른 제한적 운영 측정

후속 요청에서 실환경 API/음악 테스트를 명시적으로 허용해 loopback origin을
비교했다. 인증값은 메모리 안에서만 사용했고 결과 파일/명령행에 남기지 않았다.
API는 getSong, 커버는 256px, 음악은 format=raw Range bytes=0-65535로 제한했다.
HTTP/1.1과 prior-knowledge H2C를 실제 협상 확인 후 순서를 교대해 각각 3회×4요청,
순차 재사용/동시성 4로 측정했다. 총 144요청은 HTTP 200/206, 오류 0.

| 시나리오 | 방식 | H1 median TTFB / 완료 ms | H2C median TTFB / 완료 ms |
| --- | --- | ---: | ---: |
| API | 순차 | 0.429 / 0.453 | 0.529 / 0.606 |
| API | 동시 4 | 1.492 / 1.524 | 2.029 / 4.354 |
| Cover | 순차 | 0.661 / 0.782 | 0.916 / 1.083 |
| Cover | 동시 4 | 3.167 / 3.821 | 3.416 / 3.879 |
| Audio 64KiB | 순차 | 0.325 / 0.402 | 0.292 / 0.497 |
| Audio 64KiB | 동시 4 | 1.212 / 1.268 | 1.616 / 1.926 |

H1/H2C 순차는 각각 라운드당 1연결을 재사용했다. 동시 4에서 H1은 4연결,
H2C는 1연결을 사용했다. 음악 순차 TTFB 차이는 0.033ms로 작고 완료 시간은
H1이 유리하다. 일반 upstream H1을 유지하고 gRPC만 H2C로 분리한다.

처음 커버 생성은 868ms로 뚜렷한 cold outlier였다. 이것은 프로토콜 간 공정한
cold-cache 비교가 아니며 이후 중앙값은 warm 응답 위주다. 단일 곡과 작은 표본,
서버의 자연 트래픽이 섞인 측정이므로 모든 재생·트랜스코딩에서 절대 최적이라고
주장하지 않는다. upstream H3는 현재 Navidrome에서 꺼져 있어 테스트하지 않았다.

## 최종 적용/검증

[CI 34406415297](https://github.com/TAE-OK-11/pingola/actions/runs/34406415297)은
전체 성공(14분 15초). publish job은 캐시 재사용으로 2분 18초였다.
검증/배포 소스는 `f28a3a0691958326227134b782845edf98f75389`이며,
운영 image digest는 `sha256:9aa10e662b2c4c4ad41d1ae4821ce81a6c72429441026d02fab5b7d79669d6ca`다.

- `/root/pingora/config/pingora.yaml`에 local-origin 설정을 적용했다.
- Compose에 새 immutable digest 및 `memswap_limit: 192m`를 반영했다.
  [Docker의 memory/swap 정의](https://docs.docker.com/engine/containers/resource_constraints/)에
  따라 memory와 합계 제한을 같게 해 proxy swap을 금지한다.
- YAML 교체 직후 새 파일의 권한 때문에 시작이 잠시 실패했다.
  공개 설정 파일을 0644로 수정해 복구했고 최종 컨테이너는 healthy다.
- 새 image는 실제 인증서/config를 이용한 별도 `--check` 16개를 통과한 뒤 적용했다.
- 실제 public TLS/H2 gRPC health가 **405 → 200, grpc-status 0**으로 바뀌었다.
- HTTP/1.1, HTTP/2, HTTP/3 모두 인증을 생략한 REST ping에 기대한 Subsonic
  missing-parameter 응답(code 10)을 전달했다. 이는 인증 성공 테스트와 구분한다.
- 최종 readback: Pingora memory.current 21.3 MiB, peak 31.1 MiB,
  swap.current=0, swap.max=0, OOM/CPU throttling=0. 순간값/컨테이너 누적값이다.
- Navidrome은 시작 시각 `2026-09-09T12:36:33Z`를 유지했다. 이 작업에서 재시작하지 않았다.

백업: `/root/pingora/backups/ops-20260909/`의 기존 Compose/config 및 적용 전
상태 manifest. 이전 image도 삭제하지 않아 rollback이 가능하다.
신규 코드의 H2 CANCEL 처리는 CI의 실제 reset 테스트와 분류 테스트로 검증했다.
자연 트래픽에서 취소 발생률/장기 p99가 얼마나 줄었는지는 별도 장기 관측이 필요하다.
