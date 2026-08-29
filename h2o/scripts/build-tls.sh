#!/usr/bin/env bash
set -euo pipefail

OUTPUT_DIR=${1:?usage: build-tls.sh OUTPUT_DIR TLS_PROVIDER ARCH_FLAGS}
TLS_PROVIDER=${2:?usage: build-tls.sh OUTPUT_DIR TLS_PROVIDER ARCH_FLAGS}
ARCH_FLAGS=${3:?missing ARCH_FLAGS}

# Crypto libraries are built without LTO. H2O's own strict-aliasing/LTO profile is
# kept separate so TLS rebuilds stay fast and link failures stay rare.
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

case "${TLS_PROVIDER}" in
  boringssl)
    BORINGSSL_VERSION=${BORINGSSL_VERSION:-b9dd520e22aad0001a31962dd277b6540fc9f1e4}
    SRC_DIR=${WORK_ROOT:-/tmp}/h2o-tls-boringssl
    rm -rf "${SRC_DIR}"
    git init "${SRC_DIR}"
    git -C "${SRC_DIR}" remote add origin https://github.com/google/boringssl.git
    git -C "${SRC_DIR}" fetch --depth=1 origin "${BORINGSSL_VERSION}"
    git -C "${SRC_DIR}" checkout --detach FETCH_HEAD
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
    AWS_LC_VERSION=${AWS_LC_VERSION:-v1.62.0}
    SRC_DIR=${WORK_ROOT:-/tmp}/h2o-tls-aws-lc
    rm -rf "${SRC_DIR}"
    git init "${SRC_DIR}"
    git -C "${SRC_DIR}" remote add origin https://github.com/aws/aws-lc.git
    git -C "${SRC_DIR}" fetch --depth=1 origin "${AWS_LC_VERSION}"
    git -C "${SRC_DIR}" checkout --detach FETCH_HEAD
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
