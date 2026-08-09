# syntax=docker/dockerfile:1.25@sha256:0adf442eae370b6087e08edc7c50b552d80ddf261576f4ebd6421006b2461f12
# check=error=true

ARG RUST_VERSION=1.97.1
ARG RUST_TARGET_TRIPLE=x86_64-unknown-linux-gnu
ARG RUST_TARGET_CPU=x86-64-v2
ARG RUST_LTO=fat
ARG RUST_CODEGEN_UNITS=1
ARG TLS_PROVIDER=boringssl
ARG PGO_MODE=off
ARG PGO_TRAIN_TARGET_CPU=x86-64-v2
ARG PGO_NATIVE_BORING=on
# Rust PGO: H2 stays deliberately strong because the downstream H3 frontend
# reaches the main proxy through an internal H2C bridge. H3 client/server and
# congestion-control paths receive dedicated profiles instead of being folded
# into one generic QUIC workload.
ARG PGO_WEIGHT_H1=60
ARG PGO_WEIGHT_H2=180
ARG PGO_WEIGHT_H3=900
ARG PGO_WEIGHT_UPSTREAM_H3_BBR2=700
ARG PGO_WEIGHT_UPSTREAM_H3_CUBIC=250
ARG PGO_WEIGHT_TLS=300
ARG PGO_WEIGHT_TAIL=80
ARG PGO_TRAIN_ROUNDS=2
ARG PGO_ECDSA_CURVE=prime256v1
# Native Clang PGO is kept separate from rustc's bundled LLVM profile. The
# workload is TLS/H2/H3 focused so BoringSSL and other target C/C++ hot paths
# are optimized without merging incompatible Rust/Clang profraw formats.
ARG PGO_NATIVE_TRAIN_ROUNDS=1
ARG BORING_PGO_WEIGHT_H2=35
ARG BORING_PGO_WEIGHT_H3=30
ARG BORING_PGO_WEIGHT_UPSTREAM_H3_BBR2=10
ARG BORING_PGO_WEIGHT_UPSTREAM_H3_CUBIC=5
ARG BORING_PGO_WEIGHT_TLS=20
ARG DEBIAN_SUITE=13

FROM rust:1.97.1-slim-trixie@sha256:69153971349358535be9821190190f026a761f690c6b58c68a914d14ab2d610a AS builder

RUN apt-get update \
    && apt-get install --yes --no-install-recommends \
        build-essential \
        ca-certificates \
        clang \
        cmake \
        curl \
        git \
        lld \
        llvm \
        nghttp2-client \
        ninja-build \
        openssl \
        perl \
        pkg-config \
    && rm -rf /var/lib/apt/lists/* \
    && git --version \
    && rustc --version \
    && cargo --version \
    && clang --version | head -n1 \
    && llvm-profdata --version | head -n1

WORKDIR /src

COPY --link Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY --link vendor ./vendor
COPY --link src ./src
COPY --link examples/http3_probe.rs ./examples/
COPY --link bench/backend.rs bench/pgo_client.rs bench/pgo_train.sh bench/pgo_train_h3.sh \
    bench/pgo_train_upstream_h3.sh bench/build_pgo.sh ./bench/

ARG RUST_TARGET_TRIPLE
ARG RUST_TARGET_CPU
ARG RUST_LTO
ARG RUST_CODEGEN_UNITS
ARG ALLOCATOR=tcmalloc
ARG TLS_PROVIDER
ARG PGO_MODE
ARG PGO_TRAIN_TARGET_CPU
ARG PGO_NATIVE_BORING
ARG PGO_WEIGHT_H1
ARG PGO_WEIGHT_H2
ARG PGO_WEIGHT_H3
ARG PGO_WEIGHT_UPSTREAM_H3_BBR2
ARG PGO_WEIGHT_UPSTREAM_H3_CUBIC
ARG PGO_WEIGHT_TLS
ARG PGO_WEIGHT_TAIL
ARG PGO_TRAIN_ROUNDS
ARG PGO_ECDSA_CURVE
ARG PGO_NATIVE_TRAIN_ROUNDS
ARG BORING_PGO_WEIGHT_H2
ARG BORING_PGO_WEIGHT_H3
ARG BORING_PGO_WEIGHT_UPSTREAM_H3_BBR2
ARG BORING_PGO_WEIGHT_UPSTREAM_H3_CUBIC
ARG BORING_PGO_WEIGHT_TLS

ENV CARGO_INCREMENTAL=0 \
    CARGO_PROFILE_RELEASE_CODEGEN_UNITS=${RUST_CODEGEN_UNITS} \
    CARGO_PROFILE_RELEASE_LTO=${RUST_LTO} \
    CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER=clang \
    CMAKE_GENERATOR=Ninja \
    RUSTFLAGS_COMMON="-C link-arg=-fuse-ld=lld -C link-arg=-Wl,--gc-sections"

RUN --mount=type=cache,id=pingora-cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=pingora-cargo-git,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,id=pingora-target-${RUST_TARGET_CPU}-${RUST_LTO}-${ALLOCATOR}-${TLS_PROVIDER}-${PGO_MODE}-${PGO_TRAIN_TARGET_CPU}-${PGO_NATIVE_BORING},target=/src/target,sharing=locked \
    set -eux; \
    case "${ALLOCATOR}" in jemalloc|tcmalloc|system-allocator) ;; *) echo "unsupported allocator: ${ALLOCATOR}" >&2; exit 2 ;; esac; \
    case "${TLS_PROVIDER}" in boringssl) ;; *) echo "unsupported TLS provider: ${TLS_PROVIDER}" >&2; exit 2 ;; esac; \
    case "${PGO_MODE}" in off|train) ;; *) echo "unsupported PGO mode: ${PGO_MODE}" >&2; exit 2 ;; esac; \
    case "${RUST_LTO}" in thin|fat) ;; *) echo "unsupported Rust LTO mode: ${RUST_LTO}" >&2; exit 2 ;; esac; \
    case "${RUST_CODEGEN_UNITS}" in 1|2|4|8|16) ;; *) echo "unsupported codegen units: ${RUST_CODEGEN_UNITS}" >&2; exit 2 ;; esac; \
    chmod 755 bench/pgo_train.sh bench/pgo_train_h3.sh bench/pgo_train_upstream_h3.sh bench/build_pgo.sh; \
    if [ "${PGO_MODE}" = off ]; then \
      case "${RUST_TARGET_CPU}" in \
        x86-64-v2) NATIVE_FLAGS='-O3 -march=x86-64-v2 -mtune=generic' ;; \
        znver1|znver2|znver3|znver4) NATIVE_FLAGS="-O3 -march=${RUST_TARGET_CPU} -mtune=${RUST_TARGET_CPU}" ;; \
        *) echo "unsupported Rust target CPU: ${RUST_TARGET_CPU}" >&2; exit 2 ;; \
      esac; \
      CARGO_TARGET_DIR=/src/target/release \
      CFLAGS="${NATIVE_FLAGS}" CXXFLAGS="${NATIVE_FLAGS}" \
      RUSTFLAGS="${RUSTFLAGS_COMMON} -C target-cpu=${RUST_TARGET_CPU}" \
        cargo build --locked --release --target "${RUST_TARGET_TRIPLE}" \
          --no-default-features --features "${ALLOCATOR},tls-${TLS_PROVIDER}"; \
      install -Dm755 "/src/target/release/${RUST_TARGET_TRIPLE}/release/pingora" /out/pingora; \
    else \
      export RUST_TARGET_TRIPLE RUST_TARGET_CPU RUST_LTO RUST_CODEGEN_UNITS ALLOCATOR TLS_PROVIDER; \
      export PGO_TRAIN_TARGET_CPU PGO_NATIVE_BORING PGO_WEIGHT_H1 PGO_WEIGHT_H2 PGO_WEIGHT_H3; \
      export PGO_WEIGHT_UPSTREAM_H3_BBR2 PGO_WEIGHT_UPSTREAM_H3_CUBIC PGO_WEIGHT_TLS PGO_WEIGHT_TAIL; \
      export PGO_TRAIN_ROUNDS PGO_ECDSA_CURVE PGO_NATIVE_TRAIN_ROUNDS; \
      export BORING_PGO_WEIGHT_H2 BORING_PGO_WEIGHT_H3 BORING_PGO_WEIGHT_UPSTREAM_H3_BBR2; \
      export BORING_PGO_WEIGHT_UPSTREAM_H3_CUBIC BORING_PGO_WEIGHT_TLS RUSTFLAGS_COMMON; \
      bench/build_pgo.sh; \
    fi

FROM debian:13-slim@sha256:020c0d20b9880058cbe785a9db107156c3c75c2ac944a6aa7ab59f2add76a7bd AS runtime

ARG BUILD_VERSION=dev
ARG BUILD_REVISION=unknown
ARG ALLOCATOR=tcmalloc
ARG TLS_PROVIDER
ARG PGO_MODE
ARG PGO_TRAIN_TARGET_CPU
ARG PGO_NATIVE_BORING
ARG PGO_WEIGHT_H1
ARG PGO_WEIGHT_H2
ARG PGO_WEIGHT_H3
ARG PGO_WEIGHT_UPSTREAM_H3_BBR2
ARG PGO_WEIGHT_UPSTREAM_H3_CUBIC
ARG PGO_WEIGHT_TLS
ARG PGO_WEIGHT_TAIL
ARG PGO_ECDSA_CURVE
ARG PGO_TRAIN_ROUNDS
ARG PGO_NATIVE_TRAIN_ROUNDS
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
      org.opencontainers.image.quic.tls.provider="boringssl" \
      org.opencontainers.image.rust.pgo="${PGO_MODE}" \
      org.opencontainers.image.native.pgo="${PGO_NATIVE_BORING}" \
      org.opencontainers.image.rust.pgo-train-target-cpu="${PGO_TRAIN_TARGET_CPU}" \
      org.opencontainers.image.rust.pgo-weight-h1="${PGO_WEIGHT_H1}" \
      org.opencontainers.image.rust.pgo-weight-h2="${PGO_WEIGHT_H2}" \
      org.opencontainers.image.rust.pgo-weight-h3="${PGO_WEIGHT_H3}" \
      org.opencontainers.image.rust.pgo-weight-upstream-h3-bbr2="${PGO_WEIGHT_UPSTREAM_H3_BBR2}" \
      org.opencontainers.image.rust.pgo-weight-upstream-h3-cubic="${PGO_WEIGHT_UPSTREAM_H3_CUBIC}" \
      org.opencontainers.image.rust.pgo-weight-tls="${PGO_WEIGHT_TLS}" \
      org.opencontainers.image.rust.pgo-weight-tail="${PGO_WEIGHT_TAIL}" \
      org.opencontainers.image.rust.pgo-ecdsa-curve="${PGO_ECDSA_CURVE}" \
      org.opencontainers.image.rust.pgo-train-rounds="${PGO_TRAIN_ROUNDS}" \
      org.opencontainers.image.native.pgo-train-rounds="${PGO_NATIVE_TRAIN_ROUNDS}" \
      org.opencontainers.image.base.name="debian:${DEBIAN_SUITE}-slim" \
      org.opencontainers.image.rust.version="${RUST_VERSION}" \
      org.opencontainers.image.rust.target="${RUST_TARGET_TRIPLE}" \
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
    && if [ -f /out/pgo-profile-summary.txt ]; then \
         install -Dm644 /out/pgo-profile-summary.txt /usr/share/doc/pingora/pgo-profile-summary.txt; \
       fi \
    && if [ -f /out/pgo-native-profile-summary.txt ]; then \
         install -Dm644 /out/pgo-native-profile-summary.txt /usr/share/doc/pingora/pgo-native-profile-summary.txt; \
       fi \
    && setcap cap_net_bind_service=+ep /usr/local/bin/pingora \
    && apt-get purge --yes --auto-remove libcap2-bin \
    && rm -rf /var/lib/apt/lists/*

COPY --link --chown=10001:10001 config/pingora.yaml /etc/pingora/pingora.yaml

USER 10001:10001
WORKDIR /tmp/pingora

EXPOSE 80/tcp 443/tcp 443/udp

# Pingora handles SIGTERM as a graceful shutdown request. Keep the image's stop
# contract explicit for Docker/Compose and other OCI runtimes.
STOPSIGNAL SIGTERM

HEALTHCHECK --interval=30s --timeout=3s --start-period=10s --retries=3 \
    CMD ["/usr/local/bin/pingora", "--healthcheck"]

ENTRYPOINT ["/usr/local/bin/pingora"]
CMD ["--config", "/etc/pingora/pingora.yaml"]
