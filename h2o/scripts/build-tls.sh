#!/usr/bin/env bash
set -euo pipefail

clone_ref() {
  local dest=$1
  local url=$2
  local ref=$3
  rm -rf "${dest}"
  git clone --depth=1 --branch "${ref}" "${url}" "${dest}"
}

OUTPUT_DIR=${1:?usage: build-tls.sh OUTPUT_DIR TLS_PROVIDER ARCH_FLAGS}
TLS_PROVIDER=${2:?usage: build-tls.sh OUTPUT_DIR TLS_PROVIDER ARCH_FLAGS}
ARCH_FLAGS=${3:?missing ARCH_FLAGS}

COMMON_CMAKE_FLAGS=(
  -DCMAKE_BUILD_TYPE=Release
  -DCMAKE_C_COMPILER=clang
  -DCMAKE_CXX_COMPILER=clang++
  -DCMAKE_C_FLAGS="-O3 ${ARCH_FLAGS} -fno-strict-aliasing"
  -DCMAKE_CXX_FLAGS="-O3 ${ARCH_FLAGS} -fno-strict-aliasing"
  -DCMAKE_EXE_LINKER_FLAGS="-fuse-ld=lld -Wl,--gc-sections"
  -DCMAKE_SHARED_LINKER_FLAGS="-fuse-ld=lld -Wl,--gc-sections"
  -DBUILD_SHARED_LIBS=OFF
)

install -d "${OUTPUT_DIR}"
ROOT=${WORK_ROOT:-/tmp}/h2o-tls-src

case "${TLS_PROVIDER}" in
  boringssl)
    BORINGSSL_REF=${BORINGSSL_REF:-main}
    SRC_DIR="${ROOT}/boringssl"
    clone_ref "${SRC_DIR}" https://github.com/google/boringssl.git "${BORINGSSL_REF}"
    cmake -G Ninja -S "${SRC_DIR}" -B "${SRC_DIR}/build" \
      "${COMMON_CMAKE_FLAGS[@]}" \
      -DCMAKE_INSTALL_PREFIX="${OUTPUT_DIR}"
    cmake --build "${SRC_DIR}/build" --target install -j "$(nproc)"
    if [[ ! -f "${OUTPUT_DIR}/lib/libdecrepit.a" && -f "${SRC_DIR}/build/libdecrepit.a" ]]; then
      install -Dm644 "${SRC_DIR}/build/libdecrepit.a" "${OUTPUT_DIR}/lib/libdecrepit.a"
    fi
    test -f "${OUTPUT_DIR}/lib/libdecrepit.a"
    ;;
  aws-lc)
    AWS_LC_REF=${AWS_LC_REF:-v1.62.0}
    SRC_DIR="${ROOT}/aws-lc"
    clone_ref "${SRC_DIR}" https://github.com/aws/aws-lc.git "${AWS_LC_REF}"
    cmake -G Ninja -S "${SRC_DIR}" -B "${SRC_DIR}/build" \
      "${COMMON_CMAKE_FLAGS[@]}" \
      -DCMAKE_INSTALL_PREFIX="${OUTPUT_DIR}" \
      -DBUILD_TESTING=OFF \
      -DBUILD_TOOL=OFF
    cmake --build "${SRC_DIR}/build" --target install -j "$(nproc)"
    ;;
  *)
    echo "unsupported TLS provider: ${TLS_PROVIDER}" >&2
    exit 2
    ;;
esac

test -f "${OUTPUT_DIR}/lib/libssl.a"
test -f "${OUTPUT_DIR}/lib/libcrypto.a"
printf 'tls_provider=%s\n' "${TLS_PROVIDER}" >"${OUTPUT_DIR}/tls-provider.txt"
