#!/usr/bin/env bash
set -uo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
BASELINE_IMAGE=${BASELINE_IMAGE:?set BASELINE_IMAGE to an exact non-PGO image digest}
PGO_IMAGE=${PGO_IMAGE:?set PGO_IMAGE to an exact PGO image digest}
OUTPUT=${PGO_BENCH_OUTPUT:-${ROOT}/bench/results/pgo-$(date -u +%Y%m%dT%H%M%SZ)}

for image in "${BASELINE_IMAGE}" "${PGO_IMAGE}"; do
  if [[ "${image}" != *@sha256:* ]]; then
    echo "benchmark images must use exact digest references: ${image}" >&2
    exit 2
  fi
  docker pull "${image}" >/dev/null
done

baseline_pgo=$(docker image inspect --format '{{index .Config.Labels "org.opencontainers.image.rust.pgo"}}' "${BASELINE_IMAGE}")
pgo_mode=$(docker image inspect --format '{{index .Config.Labels "org.opencontainers.image.rust.pgo"}}' "${PGO_IMAGE}")
if [[ "${baseline_pgo}" != off || "${pgo_mode}" != train ]]; then
  echo "PGO label mismatch: baseline=${baseline_pgo} pgo=${pgo_mode}" >&2
  exit 2
fi

# Reuse the mature two-image runner. Both variants use tcmalloc; the historical
# allocator slot names are translated into baseline and PGO result files below.
export JEMALLOC_IMAGE=${BASELINE_IMAGE}
export TCMALLOC_IMAGE=${PGO_IMAGE}
export ALLOCATOR_BENCH_JEMALLOC_EXPECTED=tcmalloc
export ALLOCATOR_BENCH_TCMALLOC_EXPECTED=tcmalloc
export ALLOCATOR_BENCH_OUTPUT=${OUTPUT}
export ALLOCATOR_BENCH_ROUNDS=${PGO_BENCH_ROUNDS:-3}
export ALLOCATOR_BENCH_CPUS=${PGO_BENCH_CPUS:-0.5}
export ALLOCATOR_BENCH_MEMORY=${PGO_BENCH_MEMORY:-1g}
export ALLOCATOR_BENCH_PROFILE=${PGO_BENCH_PROFILE:-standard}

set +e
"${ROOT}/bench/allocator_images.sh"
rc=$?
set -e

cat >>"${OUTPUT}/environment.txt" <<EOF
variant_first_label=jemalloc
variant_first_pgo=off
variant_first_image=${BASELINE_IMAGE}
variant_second_label=tcmalloc
variant_second_pgo=train
variant_second_image=${PGO_IMAGE}
summary_delta_definition=pgo relative to non-pgo baseline
EOF

if [[ -f "${OUTPUT}/results.tsv" ]]; then
  awk -F '\t' 'BEGIN {OFS="\t"} NR == 1 {$1="pgo_variant"} NR > 1 && $1 == "jemalloc" {$1="baseline"} NR > 1 && $1 == "tcmalloc" {$1="pgo"} {print}' \
    "${OUTPUT}/results.tsv" >"${OUTPUT}/pgo-results.tsv"
fi
if [[ -f "${OUTPUT}/summary.tsv" ]]; then
  sed -e 's/jemalloc/baseline/g' -e 's/tcmalloc/pgo/g' \
    "${OUTPUT}/summary.tsv" >"${OUTPUT}/pgo-summary.tsv"
fi
if [[ -f "${OUTPUT}/summary.txt" ]]; then
  sed -e 's/tcmalloc/pgo/g' \
    "${OUTPUT}/summary.txt" >"${OUTPUT}/pgo-summary.txt"
  cat "${OUTPUT}/pgo-summary.txt"
fi

echo "pgo_results=${OUTPUT} exit_code=${rc}"
exit "${rc}"
