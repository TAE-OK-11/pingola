#!/usr/bin/env bash
set -euo pipefail

OUTPUT_DIR=${1:?usage: build-jemalloc.sh OUTPUT_DIR ARCH_FLAGS}
ARCH_FLAGS=${2:?missing ARCH_FLAGS}
JEMALLOC_VERSION=${JEMALLOC_VERSION:?JEMALLOC_VERSION is required}
LTO_FLAGS=$(/usr/local/bin/resolve-lto-flags.sh)
SRC_DIR=${WORK_ROOT:-/tmp}/h2o-jemalloc-src/jemalloc

COMMON_CFLAGS="-O3 ${ARCH_FLAGS} ${LTO_FLAGS} -fno-strict-aliasing -ffunction-sections -fdata-sections"
LINK_FLAGS="-fuse-ld=lld -Wl,--gc-sections ${LTO_FLAGS}"

rm -rf "${SRC_DIR}"
git clone --depth=1 --branch "${JEMALLOC_VERSION}" \
  https://github.com/jemalloc/jemalloc.git "${SRC_DIR}"

install -d "${OUTPUT_DIR}"
(
  cd "${SRC_DIR}"
  ./autogen.sh
  CC=clang CXX=clang++ AR=llvm-ar RANLIB=llvm-ranlib \
    CFLAGS="${COMMON_CFLAGS}" CXXFLAGS="${COMMON_CFLAGS}" \
    LDFLAGS="${LINK_FLAGS}" \
    ./configure \
      --disable-cxx \
      --enable-static \
      --disable-shared \
      --with-jemalloc-prefix= \
      --prefix="${OUTPUT_DIR}"
  make -j "$(nproc)" install
)

printf -- '-Wl,-Bstatic %s/lib/libjemalloc.a -Wl,-Bdynamic\n' "${OUTPUT_DIR}" \
  >"${OUTPUT_DIR}/linker.flags"
{
  printf 'allocator=jemalloc\n'
  printf 'jemalloc=%s\n' "${JEMALLOC_VERSION}"
  printf 'lto=%s\n' "${LTO_MODE:-fat}"
} >"${OUTPUT_DIR}/allocator.txt"
