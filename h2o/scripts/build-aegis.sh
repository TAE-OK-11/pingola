#!/usr/bin/env bash
set -euo pipefail

OUTPUT_DIR=${1:?usage: build-aegis.sh OUTPUT_DIR ARCH_FLAGS}
ARCH_FLAGS=${2:?missing ARCH_FLAGS}

LIBAEGIS_REF=${LIBAEGIS_REF:-HEAD}
SRC_DIR=${WORK_ROOT:-/tmp}/h2o-libaegis
rm -rf "${SRC_DIR}"
git init "${SRC_DIR}"
git -C "${SRC_DIR}" remote add origin https://github.com/jedisct1/libaegis.git
git -C "${SRC_DIR}" fetch --depth=1 origin "${LIBAEGIS_REF}"
git -C "${SRC_DIR}" checkout --detach FETCH_HEAD
cmake -G Ninja -S "${SRC_DIR}" -B "${SRC_DIR}/build" \
  -DCMAKE_BUILD_TYPE=Release \
  -DCMAKE_C_COMPILER=clang \
  -DCMAKE_CXX_COMPILER=clang++ \
  -DCMAKE_C_FLAGS="-O3 ${ARCH_FLAGS} -fno-strict-aliasing" \
  -DCMAKE_INSTALL_PREFIX="${OUTPUT_DIR}"
cmake --build "${SRC_DIR}/build" --target install -j "$(nproc)"

test -f "${OUTPUT_DIR}/lib/libaegis.a"
