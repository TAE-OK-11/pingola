#!/usr/bin/env bash
set -euo pipefail

# cc-rs/cmake can translate compatible Cargo RUSTFLAGS into C/C++ flags. Keep
# the Rust profile under /src/pgo-data out of native objects so Rust and Clang
# PGO data stay isolated. Native PGO flags under /src/pgo-native are preserved.
compiler=${PGO_REAL_NATIVE_COMPILER:-clang}
filtered=()
skip_next=false
for arg in "$@"; do
  if [[ "${skip_next}" == true ]]; then
    skip_next=false
    case "${arg}" in
      /src/pgo-data|/src/pgo-data/*) continue ;;
    esac
    filtered+=("${arg}")
    continue
  fi

  case "${arg}" in
    -fprofile-generate=/src/pgo-data|-fprofile-generate=/src/pgo-data/*|
    -fprofile-use=/src/pgo-data|-fprofile-use=/src/pgo-data/*|
    -fprofile-instr-generate=/src/pgo-data|-fprofile-instr-generate=/src/pgo-data/*|
    -fprofile-instr-use=/src/pgo-data|-fprofile-instr-use=/src/pgo-data/*)
      continue
      ;;
    -fprofile-generate|-fprofile-use|-fprofile-instr-generate|-fprofile-instr-use)
      # Rust/cargo currently emits the =PATH form, but handle the split form as
      # well without suppressing a genuine native profile path.
      filtered+=("${arg}")
      skip_next=true
      ;;
    *)
      filtered+=("${arg}")
      ;;
  esac
done

exec "${compiler}" "${filtered[@]}"
