#!/usr/bin/env bash
set -euo pipefail

clone_ref() {
  local dest=$1
  local url=$2
  local ref=$3
  rm -rf "${dest}"
  git clone --depth=1 --branch "${ref}" "${url}" "${dest}"
}

OUTPUT_DIR=${1:?usage: build-allocator.sh OUTPUT_DIR ALLOCATOR ARCH_FLAGS}
ALLOCATOR=${2:?usage: build-allocator.sh OUTPUT_DIR ALLOCATOR ARCH_FLAGS}
ARCH_FLAGS=${3:?missing ARCH_FLAGS}

install -d "${OUTPUT_DIR}"
ROOT=${WORK_ROOT:-/tmp}/h2o-allocator-src

case "${ALLOCATOR}" in
  system)
    printf 'allocator=system\n' >"${OUTPUT_DIR}/allocator.txt"
    printf '\n' >"${OUTPUT_DIR}/linker.flags"
    exit 0
    ;;
  jemalloc)
    JEMALLOC_REF=${JEMALLOC_REF:-5.3.0}
    SRC_DIR="${ROOT}/jemalloc"
    clone_ref "${SRC_DIR}" https://github.com/jemalloc/jemalloc.git "${JEMALLOC_REF}"
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
    printf -- '-L%s/lib -Wl,-Bstatic -ljemalloc -Wl,-Bdynamic\n' "${OUTPUT_DIR}" \
      >"${OUTPUT_DIR}/linker.flags"
    ;;
  tcmalloc)
    GPERFTOOLS_REF=${GPERFTOOLS_REF:-gperftools-2.16}
    SRC_DIR="${ROOT}/gperftools"
    clone_ref "${SRC_DIR}" https://github.com/gperftools/gperftools.git "${GPERFTOOLS_REF}"
    (
      cd "${SRC_DIR}"
      CC=clang CXX=clang++ CFLAGS="-O3 ${ARCH_FLAGS} -fno-strict-aliasing" \
        CXXFLAGS="-O3 ${ARCH_FLAGS} -fno-strict-aliasing" \
        ./configure \
          --enable-minimal \
          --disable-shared \
          --prefix="${OUTPUT_DIR}"
      make -j "$(nproc)" install
    )
    printf -- '-L%s/lib -Wl,-Bstatic -ltcmalloc_minimal -Wl,-Bdynamic\n' "${OUTPUT_DIR}" \
      >"${OUTPUT_DIR}/linker.flags"
    ;;
  mimalloc)
    MIMALLOC_REF=${MIMALLOC_REF:-v3.1.5}
    SRC_DIR="${ROOT}/mimalloc"
    clone_ref "${SRC_DIR}" https://github.com/microsoft/mimalloc.git "${MIMALLOC_REF}"
    cmake -G Ninja -S "${SRC_DIR}" -B "${SRC_DIR}/build" \
      -DCMAKE_BUILD_TYPE=Release \
      -DCMAKE_C_COMPILER=clang \
      -DCMAKE_CXX_COMPILER=clang++ \
      -DCMAKE_C_FLAGS="-O3 ${ARCH_FLAGS} -fno-strict-aliasing" \
      -DCMAKE_CXX_FLAGS="-O3 ${ARCH_FLAGS} -fno-strict-aliasing" \
      -DCMAKE_INSTALL_PREFIX="${OUTPUT_DIR}" \
      -DMI_BUILD_SHARED=OFF \
      -DMI_BUILD_STATIC=ON \
      -DMI_BUILD_OBJECT=OFF \
      -DMI_BUILD_TESTS=OFF \
      -DMI_OVERRIDE=ON
    cmake --build "${SRC_DIR}/build" --target install -j "$(nproc)"
    printf -- '-L%s/lib -Wl,-Bstatic -lmimalloc -Wl,-Bdynamic\n' "${OUTPUT_DIR}" \
      >"${OUTPUT_DIR}/linker.flags"
    ;;
  *)
    echo "unsupported allocator: ${ALLOCATOR}" >&2
    exit 2
    ;;
esac

printf 'allocator=%s\n' "${ALLOCATOR}" >"${OUTPUT_DIR}/allocator.txt"
