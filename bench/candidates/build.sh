#!/usr/bin/env bash
set -euo pipefail

CANDIDATE=${1:?usage: build.sh CANDIDATE OUTPUT_DIR}
OUTPUT_DIR=${2:?usage: build.sh CANDIDATE OUTPUT_DIR}
WORK_ROOT=${RUNNER_TEMP:-/tmp}/pingora-candidate-${CANDIDATE}
SOURCE_DIR=${WORK_ROOT}/source
TARGET_DIR=${WORK_ROOT}/target

case "${CANDIDATE}" in
  pingap)
    repository=https://github.com/vicanso/pingap.git
    revision=c0554b2c82f5d90502a388b494bdc6d7ede55865
    package=pingap
    binary=pingap
    ;;
  aralez)
    repository=https://github.com/sadoyan/aralez.git
    revision=c89fe48e4430903a096188f15f5f4c031918a48d
    package=aralez
    binary=aralez
    ;;
  river)
    repository=https://github.com/memorysafety/river.git
    revision=0b09036f16d8a2bfe4c6965e3f7acb7f245b509f
    package=river
    binary=river
    ;;
  pingpong)
    repository=https://github.com/Bluemangoo/Pingpong.git
    revision=13174d0321f1c384f3c75079b557682b6875d595
    package=pingpong
    binary=pingpong
    ;;
  zentinel)
    repository=https://github.com/zentinelproxy/zentinel.git
    revision=33ffdecb4a13702fa2bd4a3d9a3840ec7f1348e8
    package=zentinel-proxy
    binary=zentinel
    ;;
  *)
    echo "unsupported candidate: ${CANDIDATE}" >&2
    exit 2
    ;;
esac

rm -rf "${WORK_ROOT}"
install -d "${SOURCE_DIR}" "${TARGET_DIR}" "${OUTPUT_DIR}"
git -C "${SOURCE_DIR}" init --quiet
git -C "${SOURCE_DIR}" remote add origin "${repository}"
git -C "${SOURCE_DIR}" fetch --quiet --depth=1 origin "${revision}"
git -C "${SOURCE_DIR}" checkout --quiet --detach FETCH_HEAD
test "$(git -C "${SOURCE_DIR}" rev-parse HEAD)" = "${revision}"

export CARGO_INCREMENTAL=0
export CARGO_TARGET_DIR="${TARGET_DIR}"
export CARGO_PROFILE_RELEASE_CODEGEN_UNITS=1
export CARGO_PROFILE_RELEASE_LTO=fat
export CARGO_PROFILE_RELEASE_PANIC=abort
export CARGO_PROFILE_RELEASE_STRIP=symbols
export CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER=clang
export CFLAGS='-O3 -march=znver1 -mtune=znver1'
export CXXFLAGS='-O3 -march=znver1 -mtune=znver1'
export RUSTFLAGS='-C target-cpu=znver1 -C link-arg=-fuse-ld=lld -C link-arg=-Wl,--gc-sections'

started=$(date -u +%Y-%m-%dT%H:%M:%SZ)
/usr/bin/time -v -o "${OUTPUT_DIR}/build-time.txt" \
  cargo build --manifest-path "${SOURCE_DIR}/Cargo.toml" \
    --release --locked --package "${package}" --bin "${binary}"
finished=$(date -u +%Y-%m-%dT%H:%M:%SZ)

install -m755 "${TARGET_DIR}/release/${binary}" "${OUTPUT_DIR}/${binary}"
sha256sum "${OUTPUT_DIR}/${binary}" >"${OUTPUT_DIR}/sha256.txt"
ldd "${OUTPUT_DIR}/${binary}" >"${OUTPUT_DIR}/ldd.txt" || true
readelf -n "${OUTPUT_DIR}/${binary}" >"${OUTPUT_DIR}/readelf-notes.txt" || true
{
  echo "candidate=${CANDIDATE}"
  echo "repository=${repository}"
  echo "revision=${revision}"
  echo "package=${package}"
  echo "binary=${binary}"
  echo "started=${started}"
  echo "finished=${finished}"
  echo "rustc=$(rustc --version)"
  echo "cargo=$(cargo --version)"
  echo "target_cpu=znver1"
  echo "lto=fat"
  echo "codegen_units=1"
  echo "cflags=${CFLAGS}"
  echo "rustflags=${RUSTFLAGS}"
  echo "source_license_files=$(find "${SOURCE_DIR}" -maxdepth 2 -type f -iname 'LICENSE*' -printf '%P,' | sed 's/,$//')"
} >"${OUTPUT_DIR}/build-manifest.txt"

"${OUTPUT_DIR}/${binary}" --version >"${OUTPUT_DIR}/version.txt" 2>&1 || true
