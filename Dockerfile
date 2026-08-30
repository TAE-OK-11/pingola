# syntax=docker/dockerfile:1.25@sha256:0adf442eae370b6087e08edc7c50b552d80ddf261576f4ebd6421006b2461f12
# check=error=true

ARG RUST_VERSION=1.98.0
ARG RUST_TARGET_TRIPLE=x86_64-unknown-linux-gnu
ARG RUST_TARGET_CPU=x86-64-v2
ARG RUST_LTO=fat
ARG RUST_CODEGEN_UNITS=1
ARG TLS_PROVIDER=boringssl
ARG DEBIAN_SUITE=13

FROM rust:${RUST_VERSION}-slim-trixie@sha256:cc0448b41c3b7b7fea44f5dc50eacba729a56db365b65b7bd5e8a82d5b3db078 AS builder

ARG DEBIAN_SUITE
ARG RUST_VERSION

RUN --mount=type=cache,id=pingora-apt-builder-${DEBIAN_SUITE},target=/var/cache/apt,sharing=locked \
    --mount=type=cache,id=pingora-apt-lists-builder-${DEBIAN_SUITE},target=/var/lib/apt/lists,sharing=locked \
    rm -f /etc/apt/apt.conf.d/docker-clean \
    && apt-get update -o Acquire::Retries=3 \
    && apt-get install --yes --no-install-recommends \
        build-essential \
        ca-certificates \
        clang \
        cmake \
        curl \
        git \
        lld \
        llvm \
        ninja-build \
        openssl \
        perl \
        pkg-config \
    && git --version \
    && rustc --version \
    && cargo --version \
    && clang --version | head -n1

WORKDIR /src

COPY --link Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY --link vendor ./vendor

ARG RUST_TARGET_TRIPLE
ARG RUST_TARGET_CPU
ARG RUST_LTO
ARG RUST_CODEGEN_UNITS
ARG ALLOCATOR=jemalloc
ARG TLS_PROVIDER

ENV CARGO_HTTP_MULTIPLEXING=true \
    CARGO_HTTP_TIMEOUT=120 \
    CARGO_INCREMENTAL=0 \
    CARGO_NET_RETRY=10 \
    CARGO_PROFILE_RELEASE_CODEGEN_UNITS=${RUST_CODEGEN_UNITS} \
    CARGO_PROFILE_RELEASE_LTO=${RUST_LTO} \
    CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER=clang \
    AR=llvm-ar \
    RANLIB=llvm-ranlib \
    CMAKE_GENERATOR=Ninja \
    RUSTFLAGS_COMMON="-C linker-plugin-lto -C link-arg=-fuse-ld=lld -C link-arg=-Wl,--gc-sections"

# Keep dependency downloads in a source-independent layer. BuildKit exports
# these caches to GitHub Actions, so source-only changes avoid registry churn.
RUN --mount=type=cache,id=pingora-cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=pingora-cargo-git,target=/usr/local/cargo/git,sharing=locked \
    cargo fetch --locked --target "${RUST_TARGET_TRIPLE}"

COPY --link src ./src
COPY --link examples/http3_probe.rs ./examples/

RUN --mount=type=cache,id=pingora-cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=pingora-cargo-git,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,id=pingora-target-rust-${RUST_VERSION}-${RUST_TARGET_CPU}-${RUST_LTO}-${ALLOCATOR}-${TLS_PROVIDER},target=/src/target,sharing=locked \
    set -eux; \
    case "${ALLOCATOR}" in jemalloc|tcmalloc|system-allocator) ;; *) echo "unsupported allocator: ${ALLOCATOR}" >&2; exit 2 ;; esac; \
    case "${TLS_PROVIDER}" in boringssl) ;; *) echo "unsupported TLS provider: ${TLS_PROVIDER}" >&2; exit 2 ;; esac; \
    case "${RUST_LTO}" in thin|fat) ;; *) echo "unsupported Rust LTO mode: ${RUST_LTO}" >&2; exit 2 ;; esac; \
    case "${RUST_CODEGEN_UNITS}" in 1|2|4|8|16) ;; *) echo "unsupported codegen units: ${RUST_CODEGEN_UNITS}" >&2; exit 2 ;; esac; \
    case "${RUST_TARGET_CPU}" in \
      x86-64-v2) NATIVE_FLAGS='-O3 -march=x86-64-v2 -mtune=generic -ffunction-sections -fdata-sections' ;; \
      *) echo "unsupported Rust target CPU: ${RUST_TARGET_CPU}" >&2; exit 2 ;; \
    esac; \
    NATIVE_LTO_FLAGS='-flto=thin'; \
    CARGO_TARGET_DIR=/src/target/release \
    CFLAGS="${NATIVE_FLAGS} ${NATIVE_LTO_FLAGS}" \
    CXXFLAGS="${NATIVE_FLAGS} ${NATIVE_LTO_FLAGS}" \
    LDFLAGS="${NATIVE_LTO_FLAGS}" \
    RUSTFLAGS="${RUSTFLAGS_COMMON} -C target-cpu=${RUST_TARGET_CPU}" \
      cargo build --locked --release --target "${RUST_TARGET_TRIPLE}" \
        --no-default-features --features "${ALLOCATOR},tls-${TLS_PROVIDER}"; \
    install -Dm755 "/src/target/release/${RUST_TARGET_TRIPLE}/release/pingora" /out/pingora

FROM debian:${DEBIAN_SUITE}-slim@sha256:3a39a0592364683e6bab97937b72cad5a8fa6dcbbee90edb3bb48c7f8e94f258 AS runtime

ARG BUILD_VERSION=dev
ARG BUILD_REVISION=unknown
ARG ALLOCATOR=jemalloc
ARG TLS_PROVIDER
ARG RUST_VERSION
ARG RUST_TARGET_TRIPLE
ARG RUST_TARGET_CPU
ARG RUST_LTO
ARG RUST_CODEGEN_UNITS
ARG DEBIAN_SUITE

LABEL org.opencontainers.image.title="Pingora" \
      org.opencontainers.image.description="High-performance JBS Pingora reverse proxy" \
      org.opencontainers.image.source="https://github.com/TAE-OK-11/pingola" \
      org.opencontainers.image.version="${BUILD_VERSION}" \
      org.opencontainers.image.revision="${BUILD_REVISION}" \
      org.opencontainers.image.allocator="${ALLOCATOR}" \
      org.opencontainers.image.tls.provider="${TLS_PROVIDER}" \
      org.opencontainers.image.http3.provider="quiche" \
      org.opencontainers.image.http3.internal-protocol="direct-gateway" \
      org.opencontainers.image.quic.tls.provider="boringssl" \
      org.opencontainers.image.base.name="debian:${DEBIAN_SUITE}-slim" \
      org.opencontainers.image.rust.version="${RUST_VERSION}" \
      org.opencontainers.image.rust.target="${RUST_TARGET_TRIPLE}" \
      org.opencontainers.image.rust.target-cpu="${RUST_TARGET_CPU}" \
      org.opencontainers.image.rust.lto="${RUST_LTO}" \
      org.opencontainers.image.rust.lto-scope="full-stack" \
      org.opencontainers.image.rust.pgo="off" \
      org.opencontainers.image.kernel.ktls="host-dependent" \
      org.opencontainers.image.kernel.udp-offload="gso-gro-txtime" \
      org.opencontainers.image.kernel.tcp-tuning="256k-buf,quickack,notsent-lowat,tfo" \
      org.opencontainers.image.kernel.tcp-fastopen="listener-backlog-64" \
      org.opencontainers.image.rust.codegen-units="${RUST_CODEGEN_UNITS}" \
      org.opencontainers.image.rust.linker="lld" \
      org.opencontainers.image.licenses="Apache-2.0"

RUN --mount=from=builder,source=/out,target=/out,ro \
    --mount=type=cache,id=pingora-apt-runtime-${DEBIAN_SUITE},target=/var/cache/apt,sharing=locked \
    --mount=type=cache,id=pingora-apt-lists-runtime-${DEBIAN_SUITE},target=/var/lib/apt/lists,sharing=locked \
    rm -f /etc/apt/apt.conf.d/docker-clean \
    && apt-get update -o Acquire::Retries=3 \
    && apt-get install --yes --no-install-recommends ca-certificates libcap2-bin libstdc++6 \
    && groupadd --gid 10001 pingora \
    && useradd --uid 10001 --gid 10001 --no-create-home --shell /usr/sbin/nologin pingora \
    && install -d -o 10001 -g 10001 /etc/pingora /tmp/pingora \
    && install -Dm755 /out/pingora /usr/local/bin/pingora \
    && setcap cap_net_bind_service=+ep /usr/local/bin/pingora \
    && apt-get purge --yes --auto-remove libcap2-bin

COPY --link --chown=10001:10001 config/pingora.yaml /etc/pingora/pingora.yaml

USER 10001:10001
WORKDIR /tmp/pingora

ENV MALLOC_CONF="narenas:1,percpu_arena:percpu,retain:false,dirty_decay_ms:1000,muzzy_decay_ms:1000,background_thread:true,tcache_max:8192"

EXPOSE 80/tcp 443/tcp 443/udp

# Pingora handles SIGTERM as a graceful shutdown request. Keep the image's stop
# contract explicit for Docker/Compose and other OCI runtimes.
STOPSIGNAL SIGTERM

HEALTHCHECK --interval=30s --timeout=3s --start-period=10s --retries=3 \
    CMD ["/usr/local/bin/pingora", "--healthcheck"]

ENTRYPOINT ["/usr/local/bin/pingora"]
CMD ["--config", "/etc/pingora/pingora.yaml"]
