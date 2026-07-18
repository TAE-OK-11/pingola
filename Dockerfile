# syntax=docker/dockerfile:1.25
# check=error=true

ARG RUST_VERSION=1.97.1
ARG RUST_TARGET_TRIPLE=x86_64-unknown-linux-gnu
ARG RUST_TARGET_CPU=x86-64-v2
ARG RUST_LTO=fat
ARG RUST_CODEGEN_UNITS=1
ARG TLS_PROVIDER=aws-lc
ARG PGO_MODE=off
ARG PGO_TRAIN_TARGET_CPU=x86-64-v2
ARG BOLT_MODE=off
ARG BOLT_TRAIN_ROUNDS=1
# Rebalanced from low-noise Oracle A/B results: favor steady-state H2,
# keep enough H1 coverage, reduce duplicated standalone TLS influence,
# and retain explicit tail-path training.
ARG PGO_WEIGHT_H1=35
ARG PGO_WEIGHT_H2=120
ARG PGO_WEIGHT_TLS=15
ARG PGO_WEIGHT_TAIL=30
ARG PGO_TRAIN_ROUNDS=2
ARG PGO_ECDSA_CURVE=prime256v1
ARG DEBIAN_SUITE=trixie

FROM rust:${RUST_VERSION}-slim-${DEBIAN_SUITE} AS builder

ARG BOLT_MODE

RUN apt-get update \
    && apt-get install --yes --no-install-recommends \
        build-essential \
        ca-certificates \
        clang \
        cmake \
        curl \
        lld \
        nghttp2-client \
        ninja-build \
        openssl \
        perl \
        pkg-config \
    && if [ "${BOLT_MODE}" = train ]; then \
         apt-get install --yes --no-install-recommends bolt-19; \
       fi \
    && rm -rf /var/lib/apt/lists/* \
    && rustc --version \
    && cargo --version

WORKDIR /src

COPY --link Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY --link vendor ./vendor
COPY --link src ./src
COPY --link bench/backend.rs bench/pgo_client.rs bench/pgo_train.sh ./bench/

ARG RUST_TARGET_TRIPLE
ARG RUST_TARGET_CPU
ARG RUST_LTO
ARG RUST_CODEGEN_UNITS
ARG ALLOCATOR=tcmalloc
ARG TLS_PROVIDER
ARG PGO_MODE
ARG PGO_TRAIN_TARGET_CPU
ARG BOLT_MODE
ARG BOLT_TRAIN_ROUNDS
ARG PGO_WEIGHT_H1
ARG PGO_WEIGHT_H2
ARG PGO_WEIGHT_TLS
ARG PGO_WEIGHT_TAIL
ARG PGO_ECDSA_CURVE
ARG PGO_TRAIN_ROUNDS

ENV CARGO_INCREMENTAL=0 \
    CARGO_PROFILE_RELEASE_CODEGEN_UNITS=${RUST_CODEGEN_UNITS} \
    CARGO_PROFILE_RELEASE_LTO=${RUST_LTO} \
    CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER=clang \
    CMAKE_GENERATOR=Ninja \
    RUSTFLAGS_COMMON="-C link-arg=-fuse-ld=lld -C link-arg=-Wl,--gc-sections"

RUN --mount=type=cache,id=pingora-cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=pingora-cargo-git,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,id=pingora-target-${RUST_TARGET_CPU}-${RUST_LTO}-${ALLOCATOR}-${TLS_PROVIDER}-${PGO_MODE}-${PGO_TRAIN_TARGET_CPU},target=/src/target,sharing=locked \
    set -eux; \
    case "${ALLOCATOR}" in \
      jemalloc|tcmalloc|system-allocator) ;; \
      *) echo "unsupported allocator: ${ALLOCATOR}" >&2; exit 2 ;; \
    esac; \
    case "${TLS_PROVIDER}" in \
      aws-lc) ;; \
      *) echo "unsupported TLS provider (AWS-LC is required): ${TLS_PROVIDER}" >&2; exit 2 ;; \
    esac; \
    case "${PGO_MODE}" in \
      off|train) ;; \
      *) echo "unsupported PGO mode: ${PGO_MODE}" >&2; exit 2 ;; \
    esac; \
    case "${BOLT_MODE}" in \
      off|train) ;; \
      *) echo "unsupported BOLT mode: ${BOLT_MODE}" >&2; exit 2 ;; \
    esac; \
    if [ "${BOLT_MODE}" = train ] && [ "${PGO_MODE}" != train ]; then \
      echo 'BOLT training requires PGO_MODE=train' >&2; \
      exit 2; \
    fi; \
    case "${PGO_TRAIN_TARGET_CPU}" in \
      native|x86-64-v2|znver1|znver2|znver3|znver4) ;; \
      *) echo "unsupported PGO training target: ${PGO_TRAIN_TARGET_CPU}" >&2; exit 2 ;; \
    esac; \
    case "${RUST_TARGET_CPU}" in \
      native|x86-64-v2|znver1|znver2|znver3|znver4) ;; \
      *) echo "unsupported Rust target CPU: ${RUST_TARGET_CPU}" >&2; exit 2 ;; \
    esac; \
    case "${RUST_TARGET_CPU}" in \
      x86-64-v2) TARGET_NATIVE_FLAGS='-O3 -march=x86-64-v2 -mtune=generic' ;; \
      native) TARGET_NATIVE_FLAGS='-O3 -march=native -mtune=native' ;; \
      *) TARGET_NATIVE_FLAGS="-O3 -march=${RUST_TARGET_CPU} -mtune=${RUST_TARGET_CPU}" ;; \
    esac; \
    case "${PGO_TRAIN_TARGET_CPU}" in \
      x86-64-v2) TRAIN_NATIVE_FLAGS='-O3 -march=x86-64-v2 -mtune=generic' ;; \
      native) TRAIN_NATIVE_FLAGS='-O3 -march=native -mtune=native' ;; \
      *) TRAIN_NATIVE_FLAGS="-O3 -march=${PGO_TRAIN_TARGET_CPU} -mtune=${PGO_TRAIN_TARGET_CPU}" ;; \
    esac; \
    case "${RUST_LTO}" in \
      thin|fat) ;; \
      *) echo "unsupported Rust LTO mode: ${RUST_LTO}" >&2; exit 2 ;; \
    esac; \
    case "${RUST_CODEGEN_UNITS}" in \
      1|2|4|8|16) ;; \
      *) echo "unsupported Rust codegen unit count: ${RUST_CODEGEN_UNITS}" >&2; exit 2 ;; \
    esac; \
    case "${PGO_ECDSA_CURVE}" in \
      prime256v1|secp384r1) ;; \
      *) echo "unsupported ECDSA curve: ${PGO_ECDSA_CURVE}" >&2; exit 2 ;; \
    esac; \
    for value in "${PGO_WEIGHT_H1}" "${PGO_WEIGHT_H2}" "${PGO_WEIGHT_TLS}" "${PGO_WEIGHT_TAIL}" "${PGO_TRAIN_ROUNDS}" "${BOLT_TRAIN_ROUNDS}"; do \
      case "${value}" in ''|*[!0-9]*) echo "PGO weights/rounds must be positive integers" >&2; exit 2 ;; esac; \
      test "${value}" -gt 0; \
    done; \
    chmod 755 bench/pgo_train.sh; \
    if [ "${PGO_MODE}" = off ]; then \
      CARGO_TARGET_DIR=/src/target/release \
      CFLAGS="${TARGET_NATIVE_FLAGS}" \
      CXXFLAGS="${TARGET_NATIVE_FLAGS}" \
      RUSTFLAGS="${RUSTFLAGS_COMMON} -C target-cpu=${RUST_TARGET_CPU}" \
        cargo build --locked --release --target "${RUST_TARGET_TRIPLE}" \
          --no-default-features --features "${ALLOCATOR},tls-${TLS_PROVIDER}"; \
      install -Dm755 "/src/target/release/${RUST_TARGET_TRIPLE}/release/pingora" /out/pingora; \
    else \
      rustup component add llvm-tools-preview; \
      rustc --edition=2021 -D warnings -C opt-level=3 -C codegen-units=1 \
        -C panic=abort -C target-cpu="${PGO_TRAIN_TARGET_CPU}" -C strip=symbols \
        bench/backend.rs -o /tmp/pgo-backend; \
      rustc --edition=2021 -D warnings -C opt-level=3 -C codegen-units=1 \
        -C panic=abort -C target-cpu="${PGO_TRAIN_TARGET_CPU}" -C strip=symbols \
        bench/pgo_client.rs -o /tmp/pgo-client; \
      rm -rf /src/pgo-data; \
      install -d /src/pgo-data/raw/h1 /src/pgo-data/raw/h2 /src/pgo-data/raw/tls /src/pgo-data/raw/tail; \
      CARGO_TARGET_DIR=/src/target/pgo-generate \
      CFLAGS="${TRAIN_NATIVE_FLAGS}" \
      CXXFLAGS="${TRAIN_NATIVE_FLAGS}" \
      RUSTFLAGS="${RUSTFLAGS_COMMON} -C target-cpu=${PGO_TRAIN_TARGET_CPU} -C profile-generate=/src/pgo-data/raw" \
        cargo build --locked --release --target "${RUST_TARGET_TRIPLE}" \
          --no-default-features --features "${ALLOCATOR},tls-${TLS_PROVIDER}"; \
      PGO_BIN="/src/target/pgo-generate/${RUST_TARGET_TRIPLE}/release/pingora"; \
      test -x "${PGO_BIN}"; \
      for round in $(seq 1 "${PGO_TRAIN_ROUNDS}"); do \
        for scenario in h1 h2 tls tail; do \
          echo "PGO training scenario=${scenario} round=${round}/${PGO_TRAIN_ROUNDS}"; \
          PGO_ECDSA_CURVE="${PGO_ECDSA_CURVE}" PGO_TRAIN_ROUND="${round}" \
            bench/pgo_train.sh "${PGO_BIN}" /tmp/pgo-backend /tmp/pgo-client \
              "/src/pgo-data/raw/${scenario}" "${scenario}"; \
        done; \
      done; \
      LLVM_PROFDATA="$(rustc --print target-libdir)/../bin/llvm-profdata"; \
      test -x "${LLVM_PROFDATA}"; \
      for scenario in h1 h2 tls tail; do \
        "${LLVM_PROFDATA}" merge --failure-mode=any \
          -o "/src/pgo-data/${scenario}.profdata" "/src/pgo-data/raw/${scenario}"/*.profraw; \
      done; \
      "${LLVM_PROFDATA}" merge \
        --weighted-input="${PGO_WEIGHT_H1},/src/pgo-data/h1.profdata" \
        --weighted-input="${PGO_WEIGHT_H2},/src/pgo-data/h2.profdata" \
        --weighted-input="${PGO_WEIGHT_TLS},/src/pgo-data/tls.profdata" \
        --weighted-input="${PGO_WEIGHT_TAIL},/src/pgo-data/tail.profdata" \
        -o /src/pgo-data/merged.profdata; \
      test -s /src/pgo-data/merged.profdata; \
      { \
        echo "weights h1=${PGO_WEIGHT_H1} h2=${PGO_WEIGHT_H2} tls=${PGO_WEIGHT_TLS} tail=${PGO_WEIGHT_TAIL} rounds=${PGO_TRAIN_ROUNDS}"; \
        "${LLVM_PROFDATA}" show --counts --covered --topn=100 /src/pgo-data/merged.profdata; \
        echo; echo "=== h1 vs h2 overlap ==="; \
        "${LLVM_PROFDATA}" overlap /src/pgo-data/h1.profdata /src/pgo-data/h2.profdata || true; \
        echo; echo "=== h2 vs tls overlap ==="; \
        "${LLVM_PROFDATA}" overlap /src/pgo-data/h2.profdata /src/pgo-data/tls.profdata || true; \
        echo; echo "=== h2 vs tail overlap ==="; \
        "${LLVM_PROFDATA}" overlap /src/pgo-data/h2.profdata /src/pgo-data/tail.profdata || true; \
      } > /src/pgo-data/profile-summary.txt; \
      PROFILE_SHA="$(sha256sum /src/pgo-data/merged.profdata | cut -d ' ' -f 1)"; \
      PROFILE_PATH="/src/pgo-data/merged-${PROFILE_SHA}.profdata"; \
      cp /src/pgo-data/merged.profdata "${PROFILE_PATH}"; \
      FINAL_RUSTFLAGS="${RUSTFLAGS_COMMON} -C target-cpu=${RUST_TARGET_CPU} -C profile-use=${PROFILE_PATH} -C llvm-args=-pgo-warn-missing-function"; \
      if [ "${BOLT_MODE}" = train ]; then \
        FINAL_RUSTFLAGS="${FINAL_RUSTFLAGS} -C link-arg=-Wl,--emit-relocs"; \
      fi; \
      CARGO_TARGET_DIR=/src/target/pgo-use \
      CARGO_PROFILE_RELEASE_STRIP="$([ "${BOLT_MODE}" = train ] && echo none || echo symbols)" \
      CFLAGS="${TARGET_NATIVE_FLAGS}" \
      CXXFLAGS="${TARGET_NATIVE_FLAGS}" \
      RUSTFLAGS="${FINAL_RUSTFLAGS}" \
        cargo build --locked --release --target "${RUST_TARGET_TRIPLE}" \
          --no-default-features --features "${ALLOCATOR},tls-${TLS_PROVIDER}"; \
      FINAL_BIN="/src/target/pgo-use/${RUST_TARGET_TRIPLE}/release/pingora"; \
      if [ "${BOLT_MODE}" = train ]; then \
        test -x /usr/bin/llvm-bolt-19; \
        test -x /usr/bin/merge-fdata-19; \
        rm -rf /src/bolt-data; \
        install -d /src/bolt-data/profile; \
        llvm-bolt-19 "${FINAL_BIN}" \
          --instrument \
          --instrumentation-file=/src/bolt-data/profile/pingora.fdata \
          --instrumentation-file-append-pid \
          -o /src/bolt-data/pingora.instrumented; \
        chmod 755 /src/bolt-data/pingora.instrumented; \
        for round in $(seq 1 "${BOLT_TRAIN_ROUNDS}"); do \
          for scenario in h1 h2 tls tail; do \
            echo "BOLT training scenario=${scenario} round=${round}/${BOLT_TRAIN_ROUNDS}"; \
            PGO_ECDSA_CURVE="${PGO_ECDSA_CURVE}" PGO_TRAIN_ROUND="${round}" \
              PGO_REQUIRE_PROFILE=false \
              bench/pgo_train.sh /src/bolt-data/pingora.instrumented \
                /tmp/pgo-backend /tmp/pgo-client \
                "/src/bolt-data/${scenario}-${round}" "${scenario}"; \
          done; \
        done; \
        test -n "$(find /src/bolt-data/profile -maxdepth 1 -type f -name 'pingora.fdata.*' -print -quit)"; \
        merge-fdata-19 /src/bolt-data/profile/pingora.fdata.* \
          > /src/bolt-data/merged.fdata; \
        test -s /src/bolt-data/merged.fdata; \
        llvm-bolt-19 "${FINAL_BIN}" \
          --data=/src/bolt-data/merged.fdata \
          --reorder-blocks=ext-tsp \
          --reorder-functions=hfsort+ \
          --split-functions \
          --split-all-cold \
          --dyno-stats \
          -o /out/pingora; \
        strip --strip-all /out/pingora; \
        { \
          echo "tool=$(llvm-bolt-19 --version | head -n 1)"; \
          echo "training_rounds=${BOLT_TRAIN_ROUNDS}"; \
          sha256sum /src/bolt-data/merged.fdata; \
        } > /out/bolt-profile-summary.txt; \
      else \
        install -Dm755 "${FINAL_BIN}" /out/pingora; \
      fi; \
      install -Dm644 /src/pgo-data/profile-summary.txt /out/pgo-profile-summary.txt; \
    fi

FROM debian:${DEBIAN_SUITE}-slim AS runtime

ARG BUILD_VERSION=dev
ARG BUILD_REVISION=unknown
ARG ALLOCATOR=tcmalloc
ARG TLS_PROVIDER
ARG PGO_MODE
ARG PGO_TRAIN_TARGET_CPU
ARG BOLT_MODE
ARG BOLT_TRAIN_ROUNDS
ARG PGO_WEIGHT_H1
ARG PGO_WEIGHT_H2
ARG PGO_WEIGHT_TLS
ARG PGO_WEIGHT_TAIL
ARG PGO_ECDSA_CURVE
ARG PGO_TRAIN_ROUNDS
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
      org.opencontainers.image.rust.pgo="${PGO_MODE}" \
      org.opencontainers.image.llvm.bolt="${BOLT_MODE}" \
      org.opencontainers.image.llvm.bolt-train-rounds="${BOLT_TRAIN_ROUNDS}" \
      org.opencontainers.image.rust.pgo-train-target-cpu="${PGO_TRAIN_TARGET_CPU}" \
      org.opencontainers.image.rust.pgo-weight-h1="${PGO_WEIGHT_H1}" \
      org.opencontainers.image.rust.pgo-weight-h2="${PGO_WEIGHT_H2}" \
      org.opencontainers.image.rust.pgo-weight-tls="${PGO_WEIGHT_TLS}" \
      org.opencontainers.image.rust.pgo-weight-tail="${PGO_WEIGHT_TAIL}" \
      org.opencontainers.image.rust.pgo-ecdsa-curve="${PGO_ECDSA_CURVE}" \
      org.opencontainers.image.rust.pgo-train-rounds="${PGO_TRAIN_ROUNDS}" \
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
    && if [ -f /out/bolt-profile-summary.txt ]; then \
         install -Dm644 /out/bolt-profile-summary.txt /usr/share/doc/pingora/bolt-profile-summary.txt; \
       fi \
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
