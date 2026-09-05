#!/usr/bin/env bash
# Rust fat-LTO and final PGO-use flags shared by Dockerfile (RUSTFLAGS_COMMON) and
# bench/build_pgo.sh. Builds run on GitHub Actions only; do not compile on the proxy.
set -euo pipefail

rust_lto_link_flags() {
  # lld: ICF dedupes identical functions, -O3/--lto-O3 maximize LTO link-time opts,
  # --lto-partitions=1 trades link RAM for cross-module inlining (OK on GHA builders).
  printf '%s' \
    '-C link-arg=-fuse-ld=lld' \
    ' -C link-arg=-Wl,--gc-sections' \
    ' -C link-arg=-Wl,--icf=safe' \
    ' -C link-arg=-Wl,-O3' \
    ' -C link-arg=-Wl,--lto-O3' \
    ' -C link-arg=-Wl,--lto-partitions=1'
}

rust_pgo_final_codegen_flags() {
  printf '%s' \
    '-C llvm-args=-pgo-warn-missing-function' \
    ' -C llvm-args=-inline-threshold=275' \
    ' -C llvm-args=-vectorize-loops' \
    ' -C llvm-args=-vectorize-slp'
}
