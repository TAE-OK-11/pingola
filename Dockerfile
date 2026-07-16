# syntax=docker/dockerfile:1.25

ARG RUST_VERSION=1.97.0
ARG RUST_TARGET_CPU=x86-64-v2
ARG RUST_LTO=fat
ARG RUST_CODEGEN_UNITS=1
ARG TLS_PROVIDER=aws-lc
ARG PGO_MODE=off
ARG PGO_TRAIN_TARGET_CPU=x86-64-v2
ARG DEBIAN_SUITE=trixie

FROM rust:${RUST_VERSION}-slim-${DEBIAN_SUITE} AS builder

RUN apt-get update \
    && apt-get install --yes --no-install-recommends \
        build-essential \
        ca-certificates \
        clang \
        cmake \
        git \
        lld \
        ninja-build \
        perl \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /src

COPY --link Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY --link vendor ./vendor
COPY --link src ./src
COPY --link bench/backend.rs bench/pgo_client.rs bench/pgo_train.sh ./bench/

ARG RUST_TARGET_CPU
ARG RUST_LTO
ARG RUST_CODEGEN_UNITS
ARG ALLOCATOR=tcmalloc
ARG TLS_PROVIDER
ARG PGO_MODE
ARG PGO_TRAIN_TARGET_CPU
ENV CARGO_INCREMENTAL=0 \
    CARGO_PROFILE_RELEASE_CODEGEN_UNITS=${RUST_CODEGEN_UNITS} \
    CARGO_PROFILE_RELEASE_LTO=${RUST_LTO} \
    CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER=clang \
    CMAKE_GENERATOR=Ninja \
    RUSTFLAGS="-C target-cpu=${RUST_TARGET_CPU} -C link-arg=-fuse-ld=lld -C link-arg=-Wl,--gc-sections"

# Do not execute a target-cpu-tuned binary in the builder: a generic CI host
# may legitimately lack znver1 instructions. The x86-64-v2 image is executed
# by tests/docker_runtime.sh; specialized images are verified on their target.
RUN --mount=type=cache,id=pingora-cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=pingora-cargo-git,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,id=pingora-target-${RUST_TARGET_CPU}-${RUST_LTO}-${ALLOCATOR}-${TLS_PROVIDER}-${PGO_MODE}-${PGO_TRAIN_TARGET_CPU},target=/src/target,sharing=locked \
    case "${ALLOCATOR}" in \
      jemalloc|tcmalloc|system-allocator) ;; \
      *) echo "unsupported allocator: ${ALLOCATOR}" >&2; exit 2 ;; \
    esac \
    && case "${TLS_PROVIDER}" in \
      aws-lc|boringssl) ;; \
      *) echo "unsupported TLS provider: ${TLS_PROVIDER}" >&2; exit 2 ;; \
    esac \
    && case "${PGO_MODE}" in \
      off|train) ;; \
      *) echo "unsupported PGO mode: ${PGO_MODE}" >&2; exit 2 ;; \
    esac \
    && case "${PGO_TRAIN_TARGET_CPU}" in \
      x86-64-v2) ;; \
      *) echo "unsupported PGO training target: ${PGO_TRAIN_TARGET_CPU}" >&2; exit 2 ;; \
    esac \
    && case "${RUST_LTO}" in \
      thin|fat) ;; \
      *) echo "unsupported Rust LTO mode: ${RUST_LTO}" >&2; exit 2 ;; \
    esac \
    && case "${RUST_CODEGEN_UNITS}" in \
      1|2|4|8|16) ;; \
      *) echo "unsupported Rust codegen unit count: ${RUST_CODEGEN_UNITS}" >&2; exit 2 ;; \
    esac \
    && if [ "${PGO_MODE}" = off ]; then \
         cargo build --locked --release --no-default-features --features "${ALLOCATOR},tls-${TLS_PROVIDER}" \
         && install -Dm755 target/release/pingora /out/pingora; \
       else \
         rustup component add llvm-tools-preview \
         && rustc --edition=2021 -D warnings -C opt-level=3 -C codegen-units=1 \
              -C panic=abort -C target-cpu="${PGO_TRAIN_TARGET_CPU}" -C strip=symbols \
              bench/backend.rs -o /tmp/pgo-backend \
         && rustc --edition=2021 -D warnings -C opt-level=3 -C codegen-units=1 \
              -C panic=abort -C target-cpu="${PGO_TRAIN_TARGET_CPU}" -C strip=symbols \
              bench/pgo_client.rs -o /tmp/pgo-client \
         && rm -rf /src/pgo-data \
         && mkdir -p /src/pgo-data/raw \
         && CARGO_TARGET_DIR=/src/target/pgo-generate \
              RUSTFLAGS="${RUSTFLAGS} -C target-cpu=${PGO_TRAIN_TARGET_CPU} -C profile-generate=/src/pgo-data/raw" \
              cargo build --locked --release --no-default-features \
                --features "${ALLOCATOR},tls-${TLS_PROVIDER}" \
         && bench/pgo_train.sh /src/target/pgo-generate/release/pingora \
              /tmp/pgo-backend /tmp/pgo-client /src/pgo-data/raw \
         && LLVM_PROFDATA="$(rustc --print target-libdir)/../bin/llvm-profdata" \
         && "${LLVM_PROFDATA}" merge -o /src/pgo-data/merged.profdata \
              /src/pgo-data/raw/*.profraw \
         && test -s /src/pgo-data/merged.profdata \
         && CARGO_TARGET_DIR=/src/target/pgo-use \
              RUSTFLAGS="${RUSTFLAGS} -C profile-use=/src/pgo-data/merged.profdata" \
              cargo build --locked --release --no-default-features \
                --features "${ALLOCATOR},tls-${TLS_PROVIDER}" \
         && install -Dm755 /src/target/pgo-use/release/pingora /out/pingora; \
       fi

FROM debian:${DEBIAN_SUITE}-slim AS runtime

ARG BUILD_VERSION=dev
ARG BUILD_REVISION=unknown
ARG ALLOCATOR=tcmalloc
ARG TLS_PROVIDER
ARG PGO_MODE
ARG PGO_TRAIN_TARGET_CPU
ARG RUST_VERSION
ARG RUST_TARGET_CPU
ARG RUST_LTO
ARG RUST_CODEGEN_UNITS
ARG DEBIAN_SUITE

LABEL org.opencontainers.image.title="Pingora" \
      org.opencontainers.image.description="High-performance JBS Pingora reverse proxy" \
      org.opencontainers.image.source="https://github.com/TAE-OK-11/pingora" \
      org.opencontainers.image.version="${BUILD_VERSION}" \
      org.opencontainers.image.revision="${BUILD_REVISION}" \
      org.opencontainers.image.allocator="${ALLOCATOR}" \
      org.opencontainers.image.tls.provider="${TLS_PROVIDER}" \
      org.opencontainers.image.rust.pgo="${PGO_MODE}" \
      org.opencontainers.image.rust.pgo-train-target-cpu="${PGO_TRAIN_TARGET_CPU}" \
      org.opencontainers.image.base.name="debian:${DEBIAN_SUITE}-slim" \
      org.opencontainers.image.rust.version="${RUST_VERSION}" \
      org.opencontainers.image.rust.target-cpu="${RUST_TARGET_CPU}" \
      org.opencontainers.image.rust.lto="${RUST_LTO}" \
      org.opencontainers.image.rust.codegen-units="${RUST_CODEGEN_UNITS}" \
      org.opencontainers.image.rust.linker="lld" \
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
