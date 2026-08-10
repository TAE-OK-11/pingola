#!/usr/bin/env bash
set -euo pipefail

# cc-rs/cmake can translate compatible Cargo RUSTFLAGS into C/C++ flags. Keep
# the Rust profile under /src/pgo-data out of native objects so Rust and Clang
# PGO data stay isolated. Native PGO flags under /src/pgo-native are preserved.
compiler=${PGO_REAL_NATIVE_COMPILER:-clang}
filtered=()
for arg in "$@"; do
  case "${arg}" in
    -fprofile-generate=/src/pgo-data*|-fprofile-use=/src/pgo-data*|-fprofile-instr-generate=/src/pgo-data*|-fprofile-instr-use=/src/pgo-data*)
      continue
      ;;
    *)
      filtered+=("${arg}")
      ;;
  esac
done

exec "${compiler}" "${filtered[@]}"
