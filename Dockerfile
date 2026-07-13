# syntax=docker/dockerfile:1.7

ARG RUST_VERSION=1.96.0

FROM rust:${RUST_VERSION}-slim-bookworm AS builder

RUN apt-get update \
    && apt-get install --yes --no-install-recommends \
        build-essential \
        autoconf \
        ca-certificates \
        cmake \
        ninja-build \
        perl \
        pkg-config \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /src

COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY vendor ./vendor
COPY src ./src

ARG RUST_TARGET_CPU=x86-64-v2
ENV CARGO_INCREMENTAL=0 \
    CMAKE_GENERATOR=Ninja \
    RUSTFLAGS="-C target-cpu=${RUST_TARGET_CPU} -C link-arg=-Wl,--gc-sections"

RUN --mount=type=cache,id=pingora-cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=pingora-cargo-git,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,id=pingora-target,target=/src/target,sharing=locked \
    cargo build --locked --release \
    && target/release/pingora --allocator-info | grep -q '^allocator=jemalloc ' \
    && install -Dm755 target/release/pingora /out/pingora

FROM debian:bookworm-slim AS runtime

ARG BUILD_VERSION=dev
ARG BUILD_REVISION=unknown

LABEL org.opencontainers.image.title="Pingora" \
      org.opencontainers.image.description="High-performance AWS-LC JBS Pingora reverse proxy" \
      org.opencontainers.image.source="https://github.com/TAE-OK-11/pingora" \
      org.opencontainers.image.version="${BUILD_VERSION}" \
      org.opencontainers.image.revision="${BUILD_REVISION}" \
      org.opencontainers.image.licenses="Apache-2.0"

RUN apt-get update \
    && apt-get install --yes --no-install-recommends ca-certificates libcap2-bin \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 10001 pingora \
    && useradd --uid 10001 --gid 10001 --no-create-home --shell /usr/sbin/nologin pingora \
    && install -d -o 10001 -g 10001 /etc/pingora /var/www/pikky /tmp/pingora

COPY --from=builder /out/pingora /usr/local/bin/pingora
COPY --chown=10001:10001 config/pingora.yaml /etc/pingora/pingora.yaml

RUN setcap cap_net_bind_service=+ep /usr/local/bin/pingora

USER 10001:10001
WORKDIR /tmp/pingora

EXPOSE 80/tcp 443/tcp

HEALTHCHECK --interval=30s --timeout=3s --start-period=10s --retries=3 \
    CMD ["/usr/local/bin/pingora", "--healthcheck"]

ENTRYPOINT ["/usr/local/bin/pingora"]
CMD ["--config", "/etc/pingora/pingora.yaml"]
