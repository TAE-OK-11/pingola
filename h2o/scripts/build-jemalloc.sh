#!/usr/bin/env bash
set -euo pipefail

OUTPUT_DIR=${1:?usage: build-jemalloc.sh OUTPUT_DIR ARCH_FLAGS}
ARCH_FLAGS=${2:?missing ARCH_FLAGS}
JEMALLOC_VERSION=${JEMALLOC_VERSION:?JEMALLOC_VERSION is required}
SRC_DIR=${WORK_ROOT:-/tmp}/h2o-jemalloc-src/jemalloc

rm -rf "${SRC_DIR}"
git clone --depth=1 --branch "${JEMALLOC_VERSION}" \
  https://github.com/jemalloc/jemalloc.git "${SRC_DIR}"

install -d "${OUTPUT_DIR}"
(
  cd "${SRC_DIR}"
  ./autogen.sh
  CC=clang CXX=clang++ CFLAGS="-O3 ${ARCH_FLAGS} -fno-strict-aliasing" \
    CXXFLAGS="-O3 ${ARCH_FLAGS} -fno-strict-aliasing" \
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
printf 'allocator=jemalloc\njemalloc=%s\n' "${JEMALLOC_VERSION}" \
  >"${OUTPUT_DIR}/allocator.txt"
