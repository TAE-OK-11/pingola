# syntax=docker/dockerfile:1.25

ARG RUST_VERSION=1.97.0
ARG RUST_TARGET_CPU=x86-64-v2
ARG RUST_LTO=fat
ARG RUST_CODEGEN_UNITS=1
ARG BOLT_ENABLED=true
ARG DEBIAN_SUITE=trixie

FROM rust:${RUST_VERSION}-slim-${DEBIAN_SUITE} AS builder

RUN apt-get update \
    && apt-get install --yes --no-install-recommends \
        build-essential \
        bolt-19 \
        ca-certificates \
        clang \
        cmake \
        curl \
        lld \
        ninja-build \
        nghttp2-client \
        openssl \
        perl \
        python3-minimal \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /src

COPY --link Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY --link vendor ./vendor
COPY --link src ./src
COPY --link bench/backend.py ./bench/backend.py
COPY --link tools/bolt-train.sh ./tools/bolt-train.sh

ARG RUST_TARGET_CPU
ARG RUST_LTO
ARG RUST_CODEGEN_UNITS
ARG BOLT_ENABLED
ARG ALLOCATOR=tcmalloc
ENV CARGO_INCREMENTAL=0 \
    CARGO_PROFILE_RELEASE_CODEGEN_UNITS=${RUST_CODEGEN_UNITS} \
    CARGO_PROFILE_RELEASE_LTO=${RUST_LTO} \
    CARGO_PROFILE_RELEASE_STRIP=none \
    CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER=clang \
    CMAKE_GENERATOR=Ninja \
    RUSTFLAGS="-C target-cpu=${RUST_TARGET_CPU} -C link-arg=-fuse-ld=lld -C link-arg=-Wl,--gc-sections -C link-arg=-Wl,--emit-relocs"

RUN --mount=type=cache,id=pingora-cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=pingora-cargo-git,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,id=pingora-target-${RUST_TARGET_CPU}-${RUST_LTO}-${ALLOCATOR},target=/src/target,sharing=locked \
    set -eu; \
    case "${ALLOCATOR}" in \
      jemalloc|tcmalloc|system-allocator) ;; \
      *) echo "unsupported allocator: ${ALLOCATOR}" >&2; exit 2 ;; \
    esac \
    && case "${RUST_LTO}" in \
      thin|fat) ;; \
      *) echo "unsupported Rust LTO mode: ${RUST_LTO}" >&2; exit 2 ;; \
    esac \
    && case "${RUST_CODEGEN_UNITS}" in \
      1|2|4|8|16) ;; \
      *) echo "unsupported Rust codegen unit count: ${RUST_CODEGEN_UNITS}" >&2; exit 2 ;; \
    esac \
    && case "${BOLT_ENABLED}" in \
      true|false) ;; \
      *) echo "BOLT_ENABLED must be true or false" >&2; exit 2 ;; \
    esac \
    && cargo build --locked --release --no-default-features --features "${ALLOCATOR}" \
    && expected="${ALLOCATOR%-allocator}" \
    && target/release/pingora --allocator-info | grep -q "^allocator=${expected}" \
    && if [ "${BOLT_ENABLED}" = true ]; then \
         readelf -S target/release/pingora | grep -q '\.rela\.text'; \
         readelf -s target/release/pingora | grep -q 'FUNC'; \
         llvm-bolt-19 target/release/pingora \
           --instrument \
           --runtime-instrumentation-lib=llvm-19/lib/libbolt_rt_instr.a \
           --instrumentation-file=/tmp/pingora-bolt.fdata \
           --instrumentation-sleep-time=1 \
           --instrumentation-no-counters-clear \
           -o /tmp/pingora-instrumented || exit $?; \
         tools/bolt-train.sh /tmp/pingora-instrumented /tmp/pingora-bolt.fdata || exit $?; \
         test -s /tmp/pingora-bolt.fdata || exit 1; \
         llvm-bolt-19 target/release/pingora \
           --data=/tmp/pingora-bolt.fdata \
           --reorder-blocks=ext-tsp \
           --reorder-functions=cdsort \
           --split-functions \
           --split-all-cold \
           --split-eh \
           --dyno-stats \
           -o /out/pingora || exit $?; \
       else \
         install -Dm755 target/release/pingora /out/pingora; \
       fi \
    && strip --strip-all /out/pingora \
    && if [ "${BOLT_ENABLED}" = true ]; then \
         readelf -S /out/pingora | grep -q '\.note\.bolt_info'; \
       fi \
    && /out/pingora --allocator-info | grep -q "^allocator=${expected}"

FROM debian:${DEBIAN_SUITE}-slim AS runtime

ARG BUILD_VERSION=dev
ARG BUILD_REVISION=unknown
ARG ALLOCATOR=tcmalloc
ARG RUST_VERSION
ARG RUST_TARGET_CPU
ARG RUST_LTO
ARG RUST_CODEGEN_UNITS
ARG BOLT_ENABLED
ARG DEBIAN_SUITE

LABEL org.opencontainers.image.title="Pingora" \
      org.opencontainers.image.description="High-performance AWS-LC JBS Pingora reverse proxy" \
      org.opencontainers.image.source="https://github.com/TAE-OK-11/pingora" \
      org.opencontainers.image.version="${BUILD_VERSION}" \
      org.opencontainers.image.revision="${BUILD_REVISION}" \
      org.opencontainers.image.allocator="${ALLOCATOR}" \
      org.opencontainers.image.base.name="debian:${DEBIAN_SUITE}-slim" \
      org.opencontainers.image.rust.version="${RUST_VERSION}" \
      org.opencontainers.image.rust.target-cpu="${RUST_TARGET_CPU}" \
      org.opencontainers.image.rust.lto="${RUST_LTO}" \
      org.opencontainers.image.rust.codegen-units="${RUST_CODEGEN_UNITS}" \
      org.opencontainers.image.rust.linker="lld" \
      org.opencontainers.image.rust.bolt="${BOLT_ENABLED}" \
      org.opencontainers.image.licenses="Apache-2.0"

RUN --mount=from=builder,source=/out,target=/out,ro \
    apt-get update \
    && apt-get install --yes --no-install-recommends ca-certificates libcap2-bin libstdc++6 \
    && groupadd --gid 10001 pingora \
    && useradd --uid 10001 --gid 10001 --no-create-home --shell /usr/sbin/nologin pingora \
    && install -d -o 10001 -g 10001 /etc/pingora /var/www/pikky /tmp/pingora \
    && install -Dm755 /out/pingora /usr/local/bin/pingora \
    && setcap cap_net_bind_service=+ep /usr/local/bin/pingora \
    && apt-get purge --yes --auto-remove libcap2-bin \
    && rm -rf /var/lib/apt/lists/*

COPY --link --chown=10001:10001 config/pingora.yaml /etc/pingora/pingora.yaml

USER 10001:10001
WORKDIR /tmp/pingora

EXPOSE 80/tcp 443/tcp

HEALTHCHECK --interval=30s --timeout=3s --start-period=10s --retries=3 \
    CMD ["/usr/local/bin/pingora", "--healthcheck"]

ENTRYPOINT ["/usr/local/bin/pingora"]
CMD ["--config", "/etc/pingora/pingora.yaml"]
