# syntax=docker/dockerfile:1.25@sha256:0adf442eae370b6087e08edc7c50b552d80ddf261576f4ebd6421006b2461f12
# check=error=true

ARG H2O_VERSION=706842c0f8c0d9422efb97a4d8ef7d6ec9df87b7
ARG H2O_REPOSITORY=https://github.com/h2o/h2o.git
ARG TARGET_CPU=x86-64-v2
ARG LTO_MODE=fat
ARG DEBIAN_SUITE=13

FROM debian:${DEBIAN_SUITE}-slim@sha256:3a39a0592364683e6bab97937b72cad5a8fa6dcbbee90edb3bb48c7f8e94f258 AS builder

ARG DEBIAN_SUITE
ARG H2O_VERSION
ARG H2O_REPOSITORY
ARG TARGET_CPU
ARG LTO_MODE

RUN --mount=type=cache,id=h2o-apt-builder-${DEBIAN_SUITE},target=/var/cache/apt,sharing=locked \
    --mount=type=cache,id=h2o-apt-lists-builder-${DEBIAN_SUITE},target=/var/lib/apt/lists,sharing=locked \
    rm -f /etc/apt/apt.conf.d/docker-clean \
    && apt-get update -o Acquire::Retries=3 \
    && apt-get install --yes --no-install-recommends \
        build-essential \
        ca-certificates \
        clang \
        cmake \
        git \
        g++ \
        libssl-dev \
        lld \
        llvm \
        ninja-build \
        perl \
        pkg-config \
        zlib1g-dev \
    && git --version \
    && clang --version | head -n1 \
    && cmake --version | head -n1

WORKDIR /src

RUN git init h2o \
    && git -C h2o remote add origin "${H2O_REPOSITORY}" \
    && git -C h2o fetch --depth=1 origin "${H2O_VERSION}" \
    && git -C h2o checkout --detach FETCH_HEAD \
    && test "$(git -C h2o rev-parse HEAD)" = "${H2O_VERSION}" \
    && git -C h2o submodule update --init --recursive --depth=1

RUN --mount=type=cache,id=h2o-cmake-build-${H2O_VERSION}-${TARGET_CPU}-${LTO_MODE},target=/src/h2o/build,sharing=locked \
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
    COMMON_FLAGS="-O3 ${ARCH_FLAGS} ${LTO_FLAGS} -fno-strict-aliasing"; \
    LINK_FLAGS="-fuse-ld=lld -Wl,--gc-sections"; \
    if [ -n "${LTO_FLAGS}" ]; then \
      LINK_FLAGS="${LINK_FLAGS} ${LTO_FLAGS}"; \
    fi; \
    cmake -G Ninja -S h2o -B h2o/build \
      -DCMAKE_BUILD_TYPE=Release \
      -DCMAKE_C_COMPILER=clang \
      -DCMAKE_CXX_COMPILER=clang++ \
      -DCMAKE_C_FLAGS="${COMMON_FLAGS}" \
      -DCMAKE_CXX_FLAGS="${COMMON_FLAGS}" \
      -DCMAKE_EXE_LINKER_FLAGS="${LINK_FLAGS}" \
      -DCMAKE_SHARED_LINKER_FLAGS="${LINK_FLAGS}" \
      -DWITH_MRUBY=OFF \
      -DWITH_DTRACE=OFF \
      -DWITH_CCACHE=OFF; \
    cmake --build h2o/build --target h2o -j "$(nproc)"; \
    install -Dm755 h2o/build/h2o /out/h2o; \
    h2o/build/h2o --version | tee /out/h2o-version.txt

FROM debian:${DEBIAN_SUITE}-slim@sha256:3a39a0592364683e6bab97937b72cad5a8fa6dcbbee90edb3bb48c7f8e94f258 AS runtime

ARG BUILD_VERSION=dev
ARG BUILD_REVISION=unknown
ARG H2O_VERSION
ARG TARGET_CPU
ARG LTO_MODE
ARG DEBIAN_SUITE

LABEL org.opencontainers.image.title="H2O" \
      org.opencontainers.image.description="High-performance JBS H2O reverse proxy" \
      org.opencontainers.image.source="https://github.com/TAE-OK-11/pingola" \
      org.opencontainers.image.version="${BUILD_VERSION}" \
      org.opencontainers.image.revision="${BUILD_REVISION}" \
      org.opencontainers.image.base.name="debian:${DEBIAN_SUITE}-slim" \
      org.opencontainers.image.h2o.version="${H2O_VERSION}" \
      org.opencontainers.image.h2o.target-cpu="${TARGET_CPU}" \
      org.opencontainers.image.h2o.lto="${LTO_MODE}" \
      org.opencontainers.image.h2o.linker="lld" \
      org.opencontainers.image.licenses="MIT"

RUN --mount=from=builder,source=/out,target=/out,ro \
    --mount=type=cache,id=h2o-apt-runtime-${DEBIAN_SUITE},target=/var/cache/apt,sharing=locked \
    --mount=type=cache,id=h2o-apt-lists-runtime-${DEBIAN_SUITE},target=/var/lib/apt/lists,sharing=locked \
    rm -f /etc/apt/apt.conf.d/docker-clean \
    && apt-get update -o Acquire::Retries=3 \
    && apt-get install --yes --no-install-recommends bash ca-certificates libcap2-bin libstdc++6 \
    && groupadd --gid 10001 h2o \
    && useradd --uid 10001 --gid 10001 --no-create-home --shell /usr/sbin/nologin h2o \
    && install -d -o 10001 -g 10001 /etc/h2o /tmp/h2o /usr/share/h2o/health \
    && install -Dm755 /out/h2o /usr/local/bin/h2o \
    && install -Dm644 /out/h2o-version.txt /usr/share/doc/h2o/version.txt \
    && install -d -o 10001 -g 10001 /usr/local/share/h2o \
    && ln -sf /etc/ssl/certs/ca-certificates.crt /usr/local/share/h2o/ca-bundle.crt \
    && setcap cap_net_bind_service=+ep /usr/local/bin/h2o \
    && apt-get purge --yes --auto-remove libcap2-bin

COPY --link --chown=10001:10001 config/h2o.conf /etc/h2o/h2o.conf
COPY --link --chown=10001:10001 health/index.txt /usr/share/h2o/health/index.txt
COPY --link --chmod=755 scripts/healthcheck.sh /usr/local/bin/h2o-healthcheck

USER 10001:10001
WORKDIR /tmp/h2o

EXPOSE 80/tcp 443/tcp 443/udp

STOPSIGNAL SIGTERM

HEALTHCHECK --interval=30s --timeout=3s --start-period=10s --retries=3 \
    CMD ["/usr/local/bin/h2o-healthcheck"]

ENTRYPOINT ["/usr/local/bin/h2o"]
CMD ["-c", "/etc/h2o/h2o.conf", "-m", "worker"]
