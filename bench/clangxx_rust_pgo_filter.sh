#!/usr/bin/env bash
set -euo pipefail
PGO_REAL_NATIVE_COMPILER=clang++ exec "$(dirname "$0")/clang_rust_pgo_filter.sh" "$@"
