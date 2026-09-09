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

검증과 배포 결과는 완료 후 추가한다.
