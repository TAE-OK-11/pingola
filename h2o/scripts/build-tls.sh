#!/usr/bin/env bash
set -euo pipefail

OUTPUT_DIR=${1:?usage: build-tls.sh OUTPUT_DIR ARCH_FLAGS}
ARCH_FLAGS=${2:?missing ARCH_FLAGS}
BORINGSSL_VERSION=${BORINGSSL_VERSION:?BORINGSSL_VERSION is required}
LTO_FLAGS=$(/usr/local/bin/resolve-lto-flags.sh)

COMMON_FLAGS="-O3 ${ARCH_FLAGS} ${LTO_FLAGS} -fno-strict-aliasing -ffunction-sections -fdata-sections"
LINK_FLAGS="-fuse-ld=lld -Wl,--gc-sections ${LTO_FLAGS}"

install -d "${OUTPUT_DIR}"
SRC_DIR=${WORK_ROOT:-/tmp}/h2o-tls-src/boringssl
rm -rf "${SRC_DIR}"
git init "${SRC_DIR}"
git -C "${SRC_DIR}" remote add origin https://github.com/google/boringssl.git
git -C "${SRC_DIR}" fetch --depth=1 origin "${BORINGSSL_VERSION}"
git -C "${SRC_DIR}" checkout --detach FETCH_HEAD
test "$(git -C "${SRC_DIR}" rev-parse HEAD)" = "${BORINGSSL_VERSION}"

cmake -G Ninja -S "${SRC_DIR}" -B "${SRC_DIR}/build" \
  -DCMAKE_BUILD_TYPE=Release \
  -DCMAKE_C_COMPILER=clang \
  -DCMAKE_CXX_COMPILER=clang++ \
  -DCMAKE_AR=llvm-ar \
  -DCMAKE_RANLIB=llvm-ranlib \
  -DCMAKE_C_FLAGS="${COMMON_FLAGS}" \
  -DCMAKE_CXX_FLAGS="${COMMON_FLAGS}" \
  -DCMAKE_EXE_LINKER_FLAGS="${LINK_FLAGS}" \
  -DCMAKE_SHARED_LINKER_FLAGS="${LINK_FLAGS}" \
  -DBUILD_SHARED_LIBS=OFF \
  -DCMAKE_INSTALL_PREFIX="${OUTPUT_DIR}"
cmake --build "${SRC_DIR}/build" --target install -j "$(nproc)"
if [[ ! -f "${OUTPUT_DIR}/lib/libdecrepit.a" && -f "${SRC_DIR}/build/libdecrepit.a" ]]; then
  install -Dm644 "${SRC_DIR}/build/libdecrepit.a" "${OUTPUT_DIR}/lib/libdecrepit.a"
fi
test -f "${OUTPUT_DIR}/lib/libdecrepit.a"
test -f "${OUTPUT_DIR}/lib/libssl.a"
test -f "${OUTPUT_DIR}/lib/libcrypto.a"
{
  printf 'tls_provider=boringssl\n'
  printf 'boringssl=%s\n' "${BORINGSSL_VERSION}"
  printf 'lto=%s\n' "${LTO_MODE:-fat}"
} >"${OUTPUT_DIR}/tls-provider.txt"
