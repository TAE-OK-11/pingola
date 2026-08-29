#!/usr/bin/env bash
set -euo pipefail

OUTPUT_DIR=${1:?usage: build-allocator.sh OUTPUT_DIR ALLOCATOR ARCH_FLAGS}
ALLOCATOR=${2:?usage: build-allocator.sh OUTPUT_DIR ALLOCATOR ARCH_FLAGS}
ARCH_FLAGS=${3:?missing ARCH_FLAGS}

install -d "${OUTPUT_DIR}"

case "${ALLOCATOR}" in
  system)
    printf 'allocator=system\n' >"${OUTPUT_DIR}/allocator.txt"
    printf '\n' >"${OUTPUT_DIR}/linker.flags"
    exit 0
    ;;
  jemalloc)
    JEMALLOC_VERSION=${JEMALLOC_VERSION:-5.3.0}
    SRC_DIR=${WORK_ROOT:-/tmp}/h2o-allocator-jemalloc
    rm -rf "${SRC_DIR}"
    git init "${SRC_DIR}"
    git -C "${SRC_DIR}" remote add origin https://github.com/jemalloc/jemalloc.git
    git -C "${SRC_DIR}" fetch --depth=1 origin "${JEMALLOC_VERSION}"
    git -C "${SRC_DIR}" checkout --detach FETCH_HEAD
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
    GPERFTOOLS_VERSION=${GPERFTOOLS_VERSION:-gperftools-2.16}
    SRC_DIR=${WORK_ROOT:-/tmp}/h2o-allocator-tcmalloc
    rm -rf "${SRC_DIR}"
    git init "${SRC_DIR}"
    git -C "${SRC_DIR}" remote add origin https://github.com/gperftools/gperftools.git
    git -C "${SRC_DIR}" fetch --depth=1 origin "${GPERFTOOLS_VERSION}"
    git -C "${SRC_DIR}" checkout --detach FETCH_HEAD
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
    MIMALLOC_VERSION=${MIMALLOC_VERSION:-v3.1.5}
    SRC_DIR=${WORK_ROOT:-/tmp}/h2o-allocator-mimalloc
    rm -rf "${SRC_DIR}"
    git init "${SRC_DIR}"
    git -C "${SRC_DIR}" remote add origin https://github.com/microsoft/mimalloc.git
    git -C "${SRC_DIR}" fetch --depth=1 origin "${MIMALLOC_VERSION}"
    git -C "${SRC_DIR}" checkout --detach FETCH_HEAD
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
