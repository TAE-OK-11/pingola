# Pingola

Cloudflare의 [Pingora](https://github.com/cloudflare/pingora)로 만든 저오버헤드
리버스 프록시입니다. 이 저장소의 기본 설정은 기존 Nginx 게이트웨이가 담당하던
PiKKY, Navidrome, Vaultwarden, CouchDB, AdGuard Home/DoH 라우팅을 1 vCPU / 1 GB
환경에서 대체하도록 구성되어 있습니다.

## 구현 범위

- AWS-LC 전용 rustls 암호화 백엔드와 TLS 1.3 전용 다운스트림
- HTTP/1.1 및 HTTP/2, 최대 동시 HTTP/2 스트림 32개
- Host allowlist와 신뢰 프록시 기반 재귀적 `X-Forwarded-For` 검증
- Navidrome 스트림/커버/API, Vaultwarden 인증/WebSocket, CouchDB 스트리밍,
  AdGuard UI/DoH별 라우팅과 타임아웃
- IP별 token-bucket 요청 제한과 활성 요청/스트림 제한
- 본문 크기 제한, upstream 응답 헤더 제거, HSTS/nosniff/frame/referrer 보안 헤더
- PiKKY 정적 파일의 SPA fallback, 경로 이탈 및 symlink 탈출 차단
- 32 MiB 제한 LRU와 gzip 레벨 3, Brotli 레벨 3, Zstd 레벨 1 압축
- 동적 압축 동시 실행 1개, 8 MiB 초과 정적 파일의 64 KiB 청크 스트리밍
- 사전 압축된 `.gz`, `.br`, `.zst` 파일 우선 사용과 ETag/Last-Modified 처리
- 단일 worker, 제한된 keepalive pool, release LTO/strip로 낮춘 CPU·메모리 오버헤드
- UID 10001 비루트 컨테이너, 읽기 전용 root filesystem, 최소 Linux capability

Pingora 0.8.1의 rustls 어댑터는 기본적으로 Ring을 고정해서 가져옵니다. 이
저장소는 해당 어댑터를 같은 API의 AWS-LC 버전으로 패치하므로 이미지에 Ring과
AWS-LC가 중복 포함되지 않습니다.

## 이미지 사용

`main` 브랜치 검증을 통과하면 GitHub Actions가 다음 이미지를 빌드하고 SBOM 및
provenance와 함께 GHCR에 게시합니다.

```bash
docker pull ghcr.io/tae-ok-11/pingola:latest
docker compose up -d
```

기본 [`docker-compose.yml`](docker-compose.yml)은 Linux host network를 사용합니다.
이는 기존 설정의 `10.77.0.1` upstream과 `127.0.0.1` Korea POP을 컨테이너에서도
같은 주소로 접근하기 위해 필요합니다. 따라서 동일 호스트의 80/tcp와 443/tcp가
비어 있어야 합니다. HTTP/3를 광고하지 않으므로 443/udp는 사용하지 않습니다.

운영 전 다음 마운트를 환경에 맞게 확인하십시오.

- `./config/pingola.yaml` → `/etc/pingola/pingola.yaml`
- `/etc/nginx/cert` → `/etc/pingola/cert`
- `/var/www/pikky` → `/var/www/pikky`

컨테이너가 UID/GID `10001:10001`로 실행되므로 인증서 체인과 개인 키는 해당
사용자가 읽을 수 있어야 합니다. 키를 world-readable로 만들지 말고 전용 그룹이나
ACL로 읽기 권한만 부여하십시오. 인증서와 키는 이미지에 포함되지 않습니다.

로컬에서 직접 빌드하려면 다음 명령을 사용합니다. 기본 CPU 타깃 `x86-64-v2`는
AMD EPYC Zen 계열을 포함합니다. 더 오래된 x86-64 CPU는 build arg를 `x86-64`로
바꾸십시오.

```bash
docker build --build-arg RUST_TARGET_CPU=x86-64-v2 -t pingola:local .
```

## 설정과 점검

기본 운영 설정은 [`config/pingola.yaml`](config/pingola.yaml)에 있습니다. 실행하지
않고 스키마와 라우팅 참조를 검사할 수 있습니다.

```bash
cargo run -- --config config/pingola.yaml --check
docker run --rm \
  -v "$PWD/config:/etc/pingola:ro" \
  ghcr.io/tae-ok-11/pingola:latest --check
```

컨테이너 healthcheck는 외부 도구 없이 바이너리 자체로 plaintext listener의
`/pingola-health`를 검사합니다.

```bash
pingola --healthcheck 127.0.0.1:80
```

설정 변경은 현재 무중단 reload 대신 컨테이너 재시작으로 반영합니다. 종료 신호를
받으면 최대 60초 동안 진행 중 요청을 정리합니다. 평상시 access log는 꺼져 있고
오류는 stderr에 기록됩니다. 문제 분석이 필요할 때만 `server.access_log: true`로
켜는 것이 저사양 환경에 유리합니다.

## 검증

```bash
cargo fmt --check
cargo test --all-targets --locked
cargo clippy --all-targets --locked -- -D warnings
tests/integration.sh
```

통합 검사는 실제 TLS listener와 테스트 backend를 띄워 HTTP/2 ALPN, TLS 1.3 성공,
TLS 1.2 거부, gzip/Zstd, HSTS, Host 차단, 308 redirect, 신뢰 IP 재작성, 413 본문
제한, 429 속도 제한, upstream `Server` 헤더 제거를 확인합니다.

## Nginx 대비 의도적인 차이

- Pingora 0.8.1은 다운스트림 HTTP/3/QUIC 서버를 제공하지 않으므로 HTTP/3와
  `Alt-Svc`는 지원하지 않습니다. HTTP/2는 완전히 지원합니다.
- 알 수 없는 HTTP Host는 Nginx의 비표준 `444` 대신 표준 `421 Misdirected Request`로
  거부합니다. rustls listener에서는 알 수 없는 SNI를 handshake 단계가 아니라
  HTTP Host 단계에서 거부합니다.
- 프록시 응답 압축은 기존 정책과 동일하게 하지 않습니다. Navidrome에만 클라이언트의
  `Accept-Encoding`을 전달하고, gzip/Brotli/Zstd는 PiKKY 정적 파일에만 적용합니다.
- 기존 DoH 설정과 동작을 맞추기 위해 `direct.tae00217.cloud` upstream 인증서 검증이
  꺼져 있습니다. 내부 CA를 배포할 수 있다면 `verify_certificate: true`로 변경하는
  것을 권장합니다.

## 라이선스

Apache-2.0. Vendored Pingora rustls 어댑터는 원본 Cloudflare 저작권과 라이선스를
그 디렉터리에 함께 보존합니다.
