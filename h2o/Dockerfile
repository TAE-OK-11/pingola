# syntax=docker/dockerfile:1.26@sha256:ecfaec9ed6d810b56388c508f4121597bfbba70d41a6dfeee4d8cad5f295fc32
# check=error=true

ARG H2O_VERSION=706842c0f8c0d9422efb97a4d8ef7d6ec9df87b7
ARG H2O_REPOSITORY=https://github.com/h2o/h2o.git
ARG BORINGSSL_VERSION=da02aac3ebb96152074ea180a5a74559b82343b1
ARG JEMALLOC_VERSION=5.3.1
ARG TARGET_CPU=x86-64-v2
ARG LTO_MODE=fat
ARG DEBIAN_SUITE=13
ARG DEBIAN_DIGEST=sha256:d7e12182ce18b85b93007c1dedf31f2d29e01ccf3182cc4017c709b6259bc132

FROM debian:13-slim@sha256:d7e12182ce18b85b93007c1dedf31f2d29e01ccf3182cc4017c709b6259bc132 AS toolchain

ARG DEBIAN_SUITE

RUN --mount=type=cache,id=h2o-apt-toolchain-${DEBIAN_SUITE},target=/var/cache/apt,sharing=locked \
    --mount=type=cache,id=h2o-apt-lists-toolchain-${DEBIAN_SUITE},target=/var/lib/apt/lists,sharing=locked \
    rm -f /etc/apt/apt.conf.d/docker-clean \
    && apt-get update -o Acquire::Retries=3 \
    && apt-get install --yes --no-install-recommends \
        autoconf \
        automake \
        build-essential \
        ca-certificates \
        clang \
        cmake \
        git \
        golang-go \
        g++ \
        libc++-dev \
        libc++abi-dev \
        libtool \
        lld \
        llvm \
        ninja-build \
        perl \
        pkg-config \
        zlib1g-dev

FROM toolchain AS tls-builder

ARG TARGET_CPU
ARG LTO_MODE
ARG BORINGSSL_VERSION

WORKDIR /build
COPY --link scripts/resolve-lto-flags.sh scripts/build-tls.sh /usr/local/bin/

RUN --mount=type=cache,id=h2o-tls-boringssl-${BORINGSSL_VERSION}-${LTO_MODE},target=/tmp/h2o-tls-cache,sharing=locked \
    chmod 755 /usr/local/bin/resolve-lto-flags.sh /usr/local/bin/build-tls.sh \
    && set -eux; \
    case "${TARGET_CPU}" in \
      x86-64-v2) ARCH_FLAGS='-march=x86-64-v2 -mtune=generic' ;; \
      *) echo "unsupported target CPU: ${TARGET_CPU}" >&2; exit 2 ;; \
    esac; \
    WORK_ROOT=/tmp/h2o-tls-cache \
      BORINGSSL_VERSION="${BORINGSSL_VERSION}" \
      LTO_MODE="${LTO_MODE}" \
      /usr/local/bin/build-tls.sh /opt/tls "${ARCH_FLAGS}"

FROM toolchain AS jemalloc-builder

ARG TARGET_CPU
ARG LTO_MODE
ARG JEMALLOC_VERSION

WORKDIR /build
COPY --link scripts/resolve-lto-flags.sh scripts/build-jemalloc.sh /usr/local/bin/

RUN --mount=type=cache,id=h2o-jemalloc-${JEMALLOC_VERSION}-${LTO_MODE},target=/tmp/h2o-jemalloc-cache,sharing=locked \
    chmod 755 /usr/local/bin/resolve-lto-flags.sh /usr/local/bin/build-jemalloc.sh \
    && set -eux; \
    case "${TARGET_CPU}" in \
      x86-64-v2) ARCH_FLAGS='-march=x86-64-v2 -mtune=generic' ;; \
      *) echo "unsupported target CPU: ${TARGET_CPU}" >&2; exit 2 ;; \
    esac; \
    WORK_ROOT=/tmp/h2o-jemalloc-cache \
      JEMALLOC_VERSION="${JEMALLOC_VERSION}" \
      LTO_MODE="${LTO_MODE}" \
      /usr/local/bin/build-jemalloc.sh /opt/jemalloc "${ARCH_FLAGS}"

FROM toolchain AS builder

ARG H2O_VERSION
ARG H2O_REPOSITORY
ARG TARGET_CPU
ARG LTO_MODE
ARG BORINGSSL_VERSION
ARG JEMALLOC_VERSION

WORKDIR /src

RUN git init h2o \
    && git -C h2o remote add origin "${H2O_REPOSITORY}" \
    && git -C h2o fetch --depth=1 origin "${H2O_VERSION}" \
    && git -C h2o checkout --detach FETCH_HEAD \
    && test "$(git -C h2o rev-parse HEAD)" = "${H2O_VERSION}" \
    && git -C h2o submodule update --init --recursive --depth=1

RUN --mount=from=tls-builder,source=/opt/tls,target=/opt/tls,ro \
    --mount=from=jemalloc-builder,source=/opt/jemalloc,target=/opt/jemalloc,ro \
    --mount=type=cache,id=h2o-cmake-${H2O_VERSION}-${BORINGSSL_VERSION}-${JEMALLOC_VERSION}-${TARGET_CPU}-${LTO_MODE},target=/src/h2o/build,sharing=locked \
    set -eux; \
    case "${TARGET_CPU}" in \
      x86-64-v2) ARCH_FLAGS='-march=x86-64-v2 -mtune=generic' ;; \
      *) echo "unsupported target CPU: ${TARGET_CPU}" >&2; exit 2 ;; \
    esac; \
    case "${LTO_MODE}" in \
      fat) LTO_FLAGS='-flto' ;; \
      thin) LTO_FLAGS='-flto=thin' ;; \
      off) LTO_FLAGS='' ;; \
      *) echo "unsupported LTO mode: ${LTO_MODE}" >&2; exit 2 ;; \
    esac; \
    JEMALLOC_LINK="$(tr -d '\n' </opt/jemalloc/linker.flags)"; \
    LINK_FLAGS="-fuse-ld=lld -Wl,--gc-sections ${LTO_FLAGS} ${JEMALLOC_LINK} -lstdc++ -lpthread -ldl -lm"; \
    COMMON_FLAGS="-O3 ${ARCH_FLAGS} ${LTO_FLAGS} -fno-strict-aliasing -ffunction-sections -fdata-sections"; \
    cmake -G Ninja -S h2o -B h2o/build \
      -DCMAKE_BUILD_TYPE=Release \
      -DCMAKE_TRY_COMPILE_TARGET_TYPE=STATIC_LIBRARY \
      -DCMAKE_C_COMPILER=clang \
      -DCMAKE_CXX_COMPILER=clang++ \
      -DCMAKE_AR=llvm-ar \
      -DCMAKE_RANLIB=llvm-ranlib \
      -DCMAKE_C_FLAGS="${COMMON_FLAGS}" \
      -DCMAKE_CXX_FLAGS="${COMMON_FLAGS}" \
      -DCMAKE_EXE_LINKER_FLAGS="${LINK_FLAGS}" \
      -DCMAKE_SHARED_LINKER_FLAGS="-fuse-ld=lld -Wl,--gc-sections ${LTO_FLAGS}" \
      -DZLIB_LIBRARY=/usr/lib/x86_64-linux-gnu/libz.so \
      -DZLIB_INCLUDE_DIR=/usr/include \
      -DOPENSSL_ROOT_DIR=/opt/tls \
      -DOPENSSL_USE_STATIC_LIBS=ON \
      -DEXTERNALPROJECT_SSL_ROOT_DIR=/opt/tls \
      -DWITH_MRUBY=OFF \
      -DWITH_DTRACE=OFF \
      -DWITH_CCACHE=OFF \
      -DWITH_FUSION=OFF \
      -DWITH_KTLS=ON \
      -DWITH_IO_URING=OFF; \
    cmake --build h2o/build --target h2o -j "$(nproc)"; \
    install -d /out; \
    install -Dm755 h2o/build/h2o /out/h2o; \
    h2o/build/h2o --version | tee /out/h2o-version.txt; \
    cp /opt/tls/tls-provider.txt /out/tls-provider.txt; \
    cp /opt/jemalloc/allocator.txt /out/allocator.txt; \
    { \
      printf 'h2o=%s\n' "${H2O_VERSION}"; \
      printf 'target_cpu=%s\n' "${TARGET_CPU}"; \
      printf 'lto=%s\n' "${LTO_MODE}"; \
      printf 'lto_scope=full-stack\n'; \
      printf 'linker=lld\n'; \
      printf 'ktls=on\n'; \
    } > /out/build-info.txt

FROM debian:13-slim@sha256:d7e12182ce18b85b93007c1dedf31f2d29e01ccf3182cc4017c709b6259bc132 AS runtime

ARG BUILD_VERSION=dev
ARG BUILD_REVISION=unknown
ARG H2O_VERSION
ARG TARGET_CPU
ARG LTO_MODE
ARG BORINGSSL_VERSION
ARG JEMALLOC_VERSION
ARG DEBIAN_SUITE
ARG DEBIAN_DIGEST

LABEL org.opencontainers.image.title="H2O" \
      org.opencontainers.image.description="Standalone JBS H2O reverse proxy" \
      org.opencontainers.image.source="https://github.com/TAE-OK-11/pingola" \
      org.opencontainers.image.version="${BUILD_VERSION}" \
      org.opencontainers.image.revision="${BUILD_REVISION}" \
      org.opencontainers.image.base.name="debian:13-slim@${DEBIAN_DIGEST}" \
      org.opencontainers.image.h2o.version="${H2O_VERSION}" \
      org.opencontainers.image.h2o.target-cpu="${TARGET_CPU}" \
      org.opencontainers.image.h2o.lto="${LTO_MODE}" \
      org.opencontainers.image.h2o.lto-scope="full-stack" \
      org.opencontainers.image.h2o.linker="lld" \
      org.opencontainers.image.tls.provider="boringssl" \
      org.opencontainers.image.tls.boringssl="${BORINGSSL_VERSION}" \
      org.opencontainers.image.allocator="jemalloc" \
      org.opencontainers.image.allocator.jemalloc="${JEMALLOC_VERSION}" \
      org.opencontainers.image.licenses="MIT"

RUN --mount=from=builder,source=/out,target=/out,ro \
    --mount=type=cache,id=h2o-apt-runtime-${DEBIAN_SUITE},target=/var/cache/apt,sharing=locked \
    --mount=type=cache,id=h2o-apt-lists-runtime-${DEBIAN_SUITE},target=/var/lib/apt/lists,sharing=locked \
    rm -f /etc/apt/apt.conf.d/docker-clean \
    && apt-get update -o Acquire::Retries=3 \
    && apt-get install --yes --no-install-recommends bash ca-certificates curl libcap2-bin libc++1 libc++abi1 libstdc++6 \
    && groupadd --gid 10001 h2o \
    && useradd --uid 10001 --gid 10001 --no-create-home --shell /usr/sbin/nologin h2o \
    && install -d -o 10001 -g 10001 /etc/h2o /tmp/h2o /usr/share/h2o/health \
    && install -Dm755 /out/h2o /usr/local/bin/h2o \
    && install -Dm644 /out/h2o-version.txt /usr/share/doc/h2o/version.txt \
    && install -Dm644 /out/tls-provider.txt /usr/share/doc/h2o/tls-provider.txt \
    && install -Dm644 /out/allocator.txt /usr/share/doc/h2o/allocator.txt \
    && install -Dm644 /out/build-info.txt /usr/share/doc/h2o/build-info.txt \
    && install -d -o 10001 -g 10001 /usr/local/share/h2o \
    && ln -sf /etc/ssl/certs/ca-certificates.crt /usr/local/share/h2o/ca-bundle.crt \
    && setcap cap_net_bind_service=+ep /usr/local/bin/h2o \
    && apt-get purge --yes --auto-remove libcap2-bin

COPY --link --chown=10001:10001 config/h2o.conf /etc/h2o/h2o.conf
COPY --link --chown=10001:10001 health/index.txt /usr/share/h2o/health/index.txt
COPY --link --chmod=755 scripts/healthcheck.sh /usr/local/bin/h2o-healthcheck
COPY --link --chmod=755 scripts/allocator-info.sh /usr/local/bin/h2o-allocator-info

ENV MALLOC_CONF="narenas:1,retain:false,dirty_decay_ms:1000,tcache_max:8192"

USER 10001:10001
WORKDIR /tmp/h2o

EXPOSE 80/tcp 443/tcp 443/udp

STOPSIGNAL SIGTERM

HEALTHCHECK --interval=30s --timeout=3s --start-period=10s --retries=3 \
    CMD ["/usr/local/bin/h2o-healthcheck"]

ENTRYPOINT ["/usr/local/bin/h2o"]
CMD ["-c", "/etc/h2o/h2o.conf", "-m", "worker"]
