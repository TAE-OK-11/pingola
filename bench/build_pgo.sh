#!/usr/bin/env bash
set -Eeuo pipefail

: "${RUST_TARGET_TRIPLE:?}"
: "${RUST_TARGET_CPU:?}"
: "${RUST_LTO:?}"
: "${RUST_CODEGEN_UNITS:?}"
: "${ALLOCATOR:?}"
: "${TLS_PROVIDER:?}"
: "${PGO_TRAIN_TARGET_CPU:?}"
: "${PGO_WEIGHT_H1:?}"
: "${PGO_WEIGHT_H2:?}"
: "${PGO_WEIGHT_H3:?}"
: "${PGO_WEIGHT_UPSTREAM_H3_BBR2:?}"
: "${PGO_WEIGHT_UPSTREAM_H3_CUBIC:?}"
: "${PGO_WEIGHT_TLS:?}"
: "${PGO_WEIGHT_TAIL:?}"
: "${PGO_TRAIN_ROUNDS:?}"
: "${PGO_ECDSA_CURVE:?}"
: "${PGO_NATIVE_BORING:?}"
: "${PGO_NATIVE_TRAIN_ROUNDS:?}"
: "${BORING_PGO_WEIGHT_H2:?}"
: "${BORING_PGO_WEIGHT_H3:?}"
: "${BORING_PGO_WEIGHT_UPSTREAM_H3_BBR2:?}"
: "${BORING_PGO_WEIGHT_UPSTREAM_H3_CUBIC:?}"
: "${BORING_PGO_WEIGHT_TLS:?}"
: "${RUSTFLAGS_COMMON:?}"

# Cargo release/pgo profiles use fat LTO. Avoid -Clinker-plugin-lto here: Debian
# builder images ship LLVM 19 while rustc 1.98 bundles LLVM 22, and mixed LTO
# bitcode breaks quiche/BoringSSL links. Rust PGO still optimizes the full Rust
# dependency graph under the same fat-LTO profile.
case "${RUSTFLAGS_COMMON}" in
  *linker-plugin-lto*)
    echo "RUSTFLAGS_COMMON must not set -Clinker-plugin-lto; use cargo fat LTO profiles instead" >&2
    exit 2
    ;;
esac

case "${RUST_TARGET_CPU}" in
  x86-64-v2) ;;
  *) echo "unsupported Rust target CPU: ${RUST_TARGET_CPU}" >&2; exit 2 ;;
esac
case "${PGO_TRAIN_TARGET_CPU}" in
  x86-64-v2) ;;
  *) echo "unsupported PGO training target: ${PGO_TRAIN_TARGET_CPU}" >&2; exit 2 ;;
esac
if [[ "${RUST_TARGET_CPU}" != "${PGO_TRAIN_TARGET_CPU}" ]]; then
  echo "PGO target mismatch: train=${PGO_TRAIN_TARGET_CPU} final=${RUST_TARGET_CPU}. Per-CPU PGO profiles may not be shared." >&2
  exit 2
fi
case "${PGO_NATIVE_BORING}" in on|off) ;; *) echo "PGO_NATIVE_BORING must be on or off" >&2; exit 2 ;; esac
case "${PGO_ECDSA_CURVE}" in prime256v1|secp384r1) ;; *) echo "unsupported ECDSA curve: ${PGO_ECDSA_CURVE}" >&2; exit 2 ;; esac

for value in \
  "${PGO_WEIGHT_H1}" "${PGO_WEIGHT_H2}" "${PGO_WEIGHT_H3}" \
  "${PGO_WEIGHT_UPSTREAM_H3_BBR2}" "${PGO_WEIGHT_UPSTREAM_H3_CUBIC}" \
  "${PGO_WEIGHT_TLS}" "${PGO_WEIGHT_TAIL}" "${PGO_TRAIN_ROUNDS}" \
  "${PGO_NATIVE_TRAIN_ROUNDS}" "${BORING_PGO_WEIGHT_H2}" "${BORING_PGO_WEIGHT_H3}" \
  "${BORING_PGO_WEIGHT_UPSTREAM_H3_BBR2}" "${BORING_PGO_WEIGHT_UPSTREAM_H3_CUBIC}" \
  "${BORING_PGO_WEIGHT_TLS}"; do
  [[ "${value}" =~ ^[1-9][0-9]*$ ]] || { echo "PGO weights/rounds must be positive integers: ${value}" >&2; exit 2; }
done

TARGET_NATIVE_FLAGS='-O3 -march=x86-64-v2 -mtune=generic'
TRAIN_NATIVE_FLAGS="${TARGET_NATIVE_FLAGS}"

rustup component add llvm-tools-preview
RUST_LLVM_PROFDATA="$(rustc --print target-libdir)/../bin/llvm-profdata"
test -x "${RUST_LLVM_PROFDATA}"

rustc --edition=2024 -D warnings -C opt-level=3 -C codegen-units=1 \
  -C panic=abort -C target-cpu="${PGO_TRAIN_TARGET_CPU}" -C strip=symbols \
  bench/backend.rs -o /tmp/pgo-backend
rustc --edition=2024 -D warnings -C opt-level=3 -C codegen-units=1 \
  -C panic=abort -C target-cpu="${PGO_TRAIN_TARGET_CPU}" -C strip=symbols \
  bench/pgo_client.rs -o /tmp/pgo-client

# Build uninstrumented helpers once. The second Pingora binary acts as an H3
# origin so upstream-H3 training does not pollute the target profile with the
# server-side origin path.
CARGO_TARGET_DIR=/src/target/pgo-tools \
CFLAGS="${TRAIN_NATIVE_FLAGS}" \
CXXFLAGS="${TRAIN_NATIVE_FLAGS}" \
RUSTFLAGS="${RUSTFLAGS_COMMON} -C target-cpu=${PGO_TRAIN_TARGET_CPU}" \
  cargo build --locked --profile pgo-tools --target "${RUST_TARGET_TRIPLE}" \
    --bin pingora --example http3_probe --no-default-features \
    --features "${ALLOCATOR},tls-${TLS_PROVIDER}"
H3_PROBE="/src/target/pgo-tools/${RUST_TARGET_TRIPLE}/pgo-tools/examples/http3_probe"
ORIGIN_BIN="/src/target/pgo-tools/${RUST_TARGET_TRIPLE}/pgo-tools/pingora"
test -x "${H3_PROBE}"
test -x "${ORIGIN_BIN}"

rm -rf /src/pgo-data
install -d \
  /src/pgo-data/raw/h1 /src/pgo-data/raw/h2 /src/pgo-data/raw/h3 \
  /src/pgo-data/raw/upstream-h3-bbr2 /src/pgo-data/raw/upstream-h3-cubic \
  /src/pgo-data/raw/tls /src/pgo-data/raw/tail

# RUSTFLAGS is intentionally global: rustc applies instrumentation to JBS
# Pingora plus Rust dependencies such as vendored Pingora, hyper/h2,
# tokio-quiche, quiche, QPACK, CUBIC and BBRv2.
CARGO_TARGET_DIR=/src/target/pgo-generate \
CFLAGS="${TRAIN_NATIVE_FLAGS}" \
CXXFLAGS="${TRAIN_NATIVE_FLAGS}" \
RUSTFLAGS="${RUSTFLAGS_COMMON} -C target-cpu=${PGO_TRAIN_TARGET_CPU} -C profile-generate=/src/pgo-data/raw" \
  cargo build --locked --profile pgo-generate --target "${RUST_TARGET_TRIPLE}" \
    --no-default-features --features "${ALLOCATOR},tls-${TLS_PROVIDER}"
PGO_BIN="/src/target/pgo-generate/${RUST_TARGET_TRIPLE}/pgo-generate/pingora"
test -x "${PGO_BIN}"

for round in $(seq 1 "${PGO_TRAIN_ROUNDS}"); do
  for scenario in h1 h2 tls tail; do
    echo "Rust PGO scenario=${scenario} round=${round}/${PGO_TRAIN_ROUNDS} cpu=${PGO_TRAIN_TARGET_CPU}"
    PGO_ECDSA_CURVE="${PGO_ECDSA_CURVE}" PGO_TRAIN_ROUND="${round}" \
      bench/pgo_train.sh "${PGO_BIN}" /tmp/pgo-backend /tmp/pgo-client \
        "/src/pgo-data/raw/${scenario}" "${scenario}"
  done

  echo "Rust PGO scenario=h3 round=${round}/${PGO_TRAIN_ROUNDS} cpu=${PGO_TRAIN_TARGET_CPU}"
  PGO_ECDSA_CURVE="${PGO_ECDSA_CURVE}" PGO_TRAIN_ROUND="${round}" \
    bench/pgo_train_h3.sh "${PGO_BIN}" /tmp/pgo-backend "${H3_PROBE}" \
      /src/pgo-data/raw/h3

  for cc in bbr2 cubic; do
    [[ "${PGO_TRAIN_FAST:-off}" == on && "${cc}" == cubic ]] && continue
    echo "Rust PGO scenario=upstream-h3-${cc} round=${round}/${PGO_TRAIN_ROUNDS} cpu=${PGO_TRAIN_TARGET_CPU}"
    PGO_ECDSA_CURVE="${PGO_ECDSA_CURVE}" PGO_TRAIN_ROUND="${round}" \
      bench/pgo_train_upstream_h3.sh "${PGO_BIN}" "${ORIGIN_BIN}" /tmp/pgo-backend \
        "${H3_PROBE}" "/src/pgo-data/raw/upstream-h3-${cc}" "${cc}"
  done
done

for scenario in h1 h2 h3 upstream-h3-bbr2 upstream-h3-cubic tls tail; do
  if [[ "${scenario}" == upstream-h3-cubic && "${PGO_TRAIN_FAST:-off}" == on ]] \
    && ! compgen -G "/src/pgo-data/raw/upstream-h3-cubic/*.profraw" >/dev/null; then
    cp "/src/pgo-data/upstream-h3-bbr2.profdata" "/src/pgo-data/upstream-h3-cubic.profdata"
    continue
  fi
  "${RUST_LLVM_PROFDATA}" merge --failure-mode=any \
    -o "/src/pgo-data/${scenario}.profdata" "/src/pgo-data/raw/${scenario}"/*.profraw
done

"${RUST_LLVM_PROFDATA}" merge \
  --weighted-input="${PGO_WEIGHT_H1},/src/pgo-data/h1.profdata" \
  --weighted-input="${PGO_WEIGHT_H2},/src/pgo-data/h2.profdata" \
  --weighted-input="${PGO_WEIGHT_H3},/src/pgo-data/h3.profdata" \
  --weighted-input="${PGO_WEIGHT_UPSTREAM_H3_BBR2},/src/pgo-data/upstream-h3-bbr2.profdata" \
  --weighted-input="${PGO_WEIGHT_UPSTREAM_H3_CUBIC},/src/pgo-data/upstream-h3-cubic.profdata" \
  --weighted-input="${PGO_WEIGHT_TLS},/src/pgo-data/tls.profdata" \
  --weighted-input="${PGO_WEIGHT_TAIL},/src/pgo-data/tail.profdata" \
  -o /src/pgo-data/merged.profdata
test -s /src/pgo-data/merged.profdata

{
  echo "cpu=${PGO_TRAIN_TARGET_CPU} rounds=${PGO_TRAIN_ROUNDS}"
  echo "weights h1=${PGO_WEIGHT_H1} h2=${PGO_WEIGHT_H2} h3=${PGO_WEIGHT_H3} upstream_h3_bbr2=${PGO_WEIGHT_UPSTREAM_H3_BBR2} upstream_h3_cubic=${PGO_WEIGHT_UPSTREAM_H3_CUBIC} tls=${PGO_WEIGHT_TLS} tail=${PGO_WEIGHT_TAIL}"
  "${RUST_LLVM_PROFDATA}" show --counts --covered --topn=150 /src/pgo-data/merged.profdata
  if [[ "${PGO_TRAIN_FAST:-off}" != on ]]; then
    for pair in 'h2 h3' 'h3 upstream-h3-bbr2' 'upstream-h3-bbr2 upstream-h3-cubic' 'upstream-h3-bbr2 tls' 'h3 tail'; do
      set -- ${pair}
      echo; echo "=== $1 vs $2 overlap ==="
      "${RUST_LLVM_PROFDATA}" overlap "/src/pgo-data/$1.profdata" "/src/pgo-data/$2.profdata" || true
    done
  fi
} > /src/pgo-data/profile-summary.txt

RUST_PROFILE_SHA="$(sha256sum /src/pgo-data/merged.profdata | cut -d ' ' -f 1)"
RUST_PROFILE_PATH="/src/pgo-data/merged-${RUST_PROFILE_SHA}.profdata"
cp /src/pgo-data/merged.profdata "${RUST_PROFILE_PATH}"

NATIVE_USE_FLAGS="${TARGET_NATIVE_FLAGS}"
if [[ "${PGO_NATIVE_BORING}" == on ]]; then
  NATIVE_LLVM_PROFDATA="$(command -v llvm-profdata)"
  test -x "${NATIVE_LLVM_PROFDATA}"
  clang --version | head -n1
  "${NATIVE_LLVM_PROFDATA}" --version | head -n1

  rm -rf /src/pgo-native
  install -d \
    /src/pgo-native/raw/h2 /src/pgo-native/raw/h3 \
    /src/pgo-native/raw/upstream-h3-bbr2 /src/pgo-native/raw/upstream-h3-cubic \
    /src/pgo-native/raw/tls

  # Keep native LLVM instrumentation separate from Rust's LLVM profile. This
  # avoids mixing profile formats from rustc's bundled LLVM with Debian Clang.
  CARGO_TARGET_DIR=/src/target/pgo-native-generate \
  CFLAGS="${TRAIN_NATIVE_FLAGS} -fprofile-instr-generate" \
  CXXFLAGS="${TRAIN_NATIVE_FLAGS} -fprofile-instr-generate" \
  RUSTFLAGS="${RUSTFLAGS_COMMON} -C target-cpu=${PGO_TRAIN_TARGET_CPU} -C link-arg=-fprofile-instr-generate" \
    cargo build --locked --profile pgo-generate --target "${RUST_TARGET_TRIPLE}" \
      --no-default-features --features "${ALLOCATOR},tls-${TLS_PROVIDER}"
  NATIVE_PGO_BIN="/src/target/pgo-native-generate/${RUST_TARGET_TRIPLE}/pgo-generate/pingora"
  test -x "${NATIVE_PGO_BIN}"

  for round in $(seq 1 "${PGO_NATIVE_TRAIN_ROUNDS}"); do
    echo "Native/Boring PGO scenario=h2 round=${round}/${PGO_NATIVE_TRAIN_ROUNDS}"
    PGO_ECDSA_CURVE="${PGO_ECDSA_CURVE}" PGO_TRAIN_ROUND="${round}" \
      bench/pgo_train.sh "${NATIVE_PGO_BIN}" /tmp/pgo-backend /tmp/pgo-client \
        /src/pgo-native/raw/h2 h2
    echo "Native/Boring PGO scenario=tls round=${round}/${PGO_NATIVE_TRAIN_ROUNDS}"
    PGO_ECDSA_CURVE="${PGO_ECDSA_CURVE}" PGO_TRAIN_ROUND="${round}" \
      bench/pgo_train.sh "${NATIVE_PGO_BIN}" /tmp/pgo-backend /tmp/pgo-client \
        /src/pgo-native/raw/tls tls
    echo "Native/Boring PGO scenario=h3 round=${round}/${PGO_NATIVE_TRAIN_ROUNDS}"
    PGO_ECDSA_CURVE="${PGO_ECDSA_CURVE}" PGO_TRAIN_ROUND="${round}" \
      bench/pgo_train_h3.sh "${NATIVE_PGO_BIN}" /tmp/pgo-backend "${H3_PROBE}" \
        /src/pgo-native/raw/h3
    for cc in bbr2 cubic; do
      echo "Native/Boring PGO scenario=upstream-h3-${cc} round=${round}/${PGO_NATIVE_TRAIN_ROUNDS}"
      PGO_ECDSA_CURVE="${PGO_ECDSA_CURVE}" PGO_TRAIN_ROUND="${round}" \
        bench/pgo_train_upstream_h3.sh "${NATIVE_PGO_BIN}" "${ORIGIN_BIN}" /tmp/pgo-backend \
          "${H3_PROBE}" "/src/pgo-native/raw/upstream-h3-${cc}" "${cc}"
    done
  done

  for scenario in h2 h3 upstream-h3-bbr2 upstream-h3-cubic tls; do
    "${NATIVE_LLVM_PROFDATA}" merge --failure-mode=any \
      -o "/src/pgo-native/${scenario}.profdata" "/src/pgo-native/raw/${scenario}"/*.profraw
  done
  "${NATIVE_LLVM_PROFDATA}" merge \
    --weighted-input="${BORING_PGO_WEIGHT_H2},/src/pgo-native/h2.profdata" \
    --weighted-input="${BORING_PGO_WEIGHT_H3},/src/pgo-native/h3.profdata" \
    --weighted-input="${BORING_PGO_WEIGHT_UPSTREAM_H3_BBR2},/src/pgo-native/upstream-h3-bbr2.profdata" \
    --weighted-input="${BORING_PGO_WEIGHT_UPSTREAM_H3_CUBIC},/src/pgo-native/upstream-h3-cubic.profdata" \
    --weighted-input="${BORING_PGO_WEIGHT_TLS},/src/pgo-native/tls.profdata" \
    -o /src/pgo-native/merged.profdata
  test -s /src/pgo-native/merged.profdata
  {
    echo "cpu=${PGO_TRAIN_TARGET_CPU} rounds=${PGO_NATIVE_TRAIN_ROUNDS}"
    echo "weights h2=${BORING_PGO_WEIGHT_H2} h3=${BORING_PGO_WEIGHT_H3} upstream_h3_bbr2=${BORING_PGO_WEIGHT_UPSTREAM_H3_BBR2} upstream_h3_cubic=${BORING_PGO_WEIGHT_UPSTREAM_H3_CUBIC} tls=${BORING_PGO_WEIGHT_TLS}"
    clang --version | head -n1
    "${NATIVE_LLVM_PROFDATA}" --version | head -n1
    "${NATIVE_LLVM_PROFDATA}" show --counts --covered --topn=150 /src/pgo-native/merged.profdata
  } > /src/pgo-native/profile-summary.txt
  NATIVE_USE_FLAGS="${TARGET_NATIVE_FLAGS} -fprofile-instr-use=/src/pgo-native/merged.profdata -Wno-profile-instr-unprofiled -Wno-profile-instr-out-of-date"
fi

FINAL_RUSTFLAGS="${RUSTFLAGS_COMMON} -C target-cpu=${RUST_TARGET_CPU} -C profile-use=${RUST_PROFILE_PATH} -C llvm-args=-pgo-warn-missing-function"
CARGO_TARGET_DIR=/src/target/pgo-use \
CFLAGS="${NATIVE_USE_FLAGS}" \
CXXFLAGS="${NATIVE_USE_FLAGS}" \
RUSTFLAGS="${FINAL_RUSTFLAGS}" \
  cargo build --locked --profile pgo --target "${RUST_TARGET_TRIPLE}" \
    --no-default-features --features "${ALLOCATOR},tls-${TLS_PROVIDER}"
FINAL_BIN="/src/target/pgo-use/${RUST_TARGET_TRIPLE}/pgo/pingora"
test -x "${FINAL_BIN}"
install -Dm755 "${FINAL_BIN}" /out/pingora
install -Dm644 /src/pgo-data/profile-summary.txt /out/pgo-profile-summary.txt
if [[ -f /src/pgo-native/profile-summary.txt ]]; then
  install -Dm644 /src/pgo-native/profile-summary.txt /out/pgo-native-profile-summary.txt
fi
