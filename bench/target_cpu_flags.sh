#!/usr/bin/env bash
# Shared Rust target-cpu validation and matching Clang C/C++ flags for BoringSSL,
# quiche, and jemalloc native objects. PGO training runs on GitHub Actions
# (x86-64-v2); final release images may target cascadelake for the proxy VM.
set -euo pipefail

validate_rust_target_cpu() {
  case "$1" in
    x86-64-v2 | cascadelake) return 0 ;;
    *)
      echo "unsupported Rust target CPU: $1 (supported: x86-64-v2, cascadelake)" >&2
      return 1
      ;;
  esac
}

rust_target_cpu_native_cflags() {
  case "$1" in
    x86-64-v2)
      printf '%s' '-O3 -march=x86-64-v2 -mtune=generic -ffunction-sections -fdata-sections'
      ;;
    cascadelake)
      # Intel Cascade Lake / Cooper Lake (8259CL proxy). Enables AVX-512 where LLVM
      # can use it. Requires AVX2 at minimum; do not run on znver1/older baseline.
      printf '%s' '-O3 -march=cascadelake -mtune=cascadelake -ffunction-sections -fdata-sections'
      ;;
    *)
      echo "unsupported Rust target CPU: $1" >&2
      return 1
      ;;
  esac
}
