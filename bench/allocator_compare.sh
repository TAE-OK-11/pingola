#!/usr/bin/env bash
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
OUTPUT=${ALLOCATOR_BENCH_OUTPUT:-${ROOT}/bench/results/allocator-$(date -u +%Y%m%dT%H%M%SZ)}
ROUNDS=${ALLOCATOR_BENCH_ROUNDS:-3}
JEMALLOC_IMAGE=${JEMALLOC_IMAGE:-pingora:allocator-jemalloc}
SYSTEM_IMAGE=${SYSTEM_IMAGE:-pingora:allocator-system}

mkdir -p "${OUTPUT}"
chmod 0755 "${OUTPUT}"

docker build --build-arg RUST_TARGET_CPU=x86-64-v2 \
  --build-arg EXPECTED_ALLOCATOR=jemalloc -t "${JEMALLOC_IMAGE}" "${ROOT}"
docker build --build-arg RUST_TARGET_CPU=x86-64-v2 \
  --build-arg 'CARGO_FEATURE_ARGS=--no-default-features --features system-allocator' \
  --build-arg EXPECTED_ALLOCATOR=system -t "${SYSTEM_IMAGE}" "${ROOT}"

set +e
BENCH_PROFILE=smoke BENCH_ROUNDS="${ROUNDS}" PINGORA_IMAGE="${JEMALLOC_IMAGE}" \
  BENCH_OUTPUT="${OUTPUT}/jemalloc" "${ROOT}/bench/compare.sh"
JEMALLOC_RC=$?
BENCH_PROFILE=smoke BENCH_ROUNDS="${ROUNDS}" PINGORA_IMAGE="${SYSTEM_IMAGE}" \
  BENCH_OUTPUT="${OUTPUT}/system" "${ROOT}/bench/compare.sh"
SYSTEM_RC=$?
set -e

python3 - "${OUTPUT}" <<'PY'
import csv
import pathlib
import statistics
import sys

root = pathlib.Path(sys.argv[1])
for allocator in ("jemalloc", "system"):
    with (root / allocator / "results.tsv").open() as source:
        rows = [row for row in csv.DictReader(source, delimiter="\t") if row["proxy"] == "pingora" and row["status"] == "PASS"]
    rps = [float(row["rps"]) for row in rows]
    p99 = [float(row["p99_us"]) for row in rows]
    rss = [float(row["rss_peak_kib"]) for row in rows]
    print(
        f"{allocator}\tcases={len(rows)}\tmedian_rps={statistics.median(rps):.2f}"
        f"\tmedian_p99_us={statistics.median(p99):.0f}\tpeak_rss_kib={max(rss):.0f}"
    )
PY

echo "allocator comparison=${OUTPUT} jemalloc_rc=${JEMALLOC_RC} system_rc=${SYSTEM_RC}"
exit $((JEMALLOC_RC != 0 || SYSTEM_RC != 0))
