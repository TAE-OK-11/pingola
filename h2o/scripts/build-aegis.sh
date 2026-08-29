#!/usr/bin/env bash
set -euo pipefail

clone_ref() {
  local dest=$1
  local url=$2
  local ref=$3
  rm -rf "${dest}"
  git clone --depth=1 --branch "${ref}" "${url}" "${dest}"
}

OUTPUT_DIR=${1:?usage: build-aegis.sh OUTPUT_DIR ARCH_FLAGS}
ARCH_FLAGS=${2:?missing ARCH_FLAGS}

LIBAEGIS_REF=${LIBAEGIS_REF:-0.1.25}
ROOT=${WORK_ROOT:-/tmp}/h2o-aegis-src
SRC_DIR="${ROOT}/libaegis"

clone_ref "${SRC_DIR}" https://github.com/jedisct1/libaegis.git "${LIBAEGIS_REF}"
cmake -G Ninja -S "${SRC_DIR}" -B "${SRC_DIR}/build" \
  -DCMAKE_BUILD_TYPE=Release \
  -DCMAKE_C_COMPILER=clang \
  -DCMAKE_CXX_COMPILER=clang++ \
  -DCMAKE_C_FLAGS="-O3 ${ARCH_FLAGS} -fno-strict-aliasing" \
  -DCMAKE_INSTALL_PREFIX="${OUTPUT_DIR}"
cmake --build "${SRC_DIR}/build" --target install -j "$(nproc)"

test -f "${OUTPUT_DIR}/lib/libaegis.a"
