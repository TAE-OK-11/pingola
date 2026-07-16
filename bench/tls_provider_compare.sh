#!/usr/bin/env bash
set -uo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
AWS_LC_IMAGE=${AWS_LC_IMAGE:?set AWS_LC_IMAGE to an exact image digest}
CLOUDFLARE_BORINGSSL_IMAGE=${CLOUDFLARE_BORINGSSL_IMAGE:?set CLOUDFLARE_BORINGSSL_IMAGE to an exact image digest}
OUTPUT=${TLS_BENCH_OUTPUT:-${ROOT}/bench/results/tls-provider-$(date -u +%Y%m%dT%H%M%SZ)}

# Reuse the two-image runner. Its first historical slot is BoringSSL and its second slot is
# AWS-LC, so the summary delta is always AWS-LC relative to Cloudflare BoringSSL.
export JEMALLOC_IMAGE=${CLOUDFLARE_BORINGSSL_IMAGE}
export TCMALLOC_IMAGE=${AWS_LC_IMAGE}
export ALLOCATOR_BENCH_JEMALLOC_EXPECTED=tcmalloc
export ALLOCATOR_BENCH_TCMALLOC_EXPECTED=tcmalloc
export ALLOCATOR_BENCH_OUTPUT=${OUTPUT}
export ALLOCATOR_BENCH_ROUNDS=${TLS_BENCH_ROUNDS:-3}
export ALLOCATOR_BENCH_CPUS=${TLS_BENCH_CPUS:-0.5}
export ALLOCATOR_BENCH_MEMORY=${TLS_BENCH_MEMORY:-1g}
export ALLOCATOR_BENCH_PROFILE=${TLS_BENCH_PROFILE:-standard}

set +e
"${ROOT}/bench/allocator_images.sh"
rc=$?
set -e

cat >>"${OUTPUT}/environment.txt" <<EOF
variant_first_label=jemalloc
variant_first_provider=cloudflare-boringssl
variant_first_image=${CLOUDFLARE_BORINGSSL_IMAGE}
variant_second_label=tcmalloc
variant_second_provider=aws-lc
variant_second_image=${AWS_LC_IMAGE}
summary_delta_definition=aws-lc relative to cloudflare-boringssl
EOF

if [[ -f "${OUTPUT}/results.tsv" ]]; then
  awk -F '\t' 'BEGIN {OFS="\t"} NR == 1 {$1="tls_provider"} NR > 1 && $1 == "jemalloc" {$1="cloudflare-boringssl"} NR > 1 && $1 == "tcmalloc" {$1="aws-lc"} {print}' \
    "${OUTPUT}/results.tsv" >"${OUTPUT}/provider-results.tsv"
fi
if [[ -f "${OUTPUT}/summary.tsv" ]]; then
  sed -e 's/jemalloc/cloudflare_boringssl/g' -e 's/tcmalloc/aws_lc/g' \
    "${OUTPUT}/summary.tsv" >"${OUTPUT}/provider-summary.tsv"
fi
if [[ -f "${OUTPUT}/summary.txt" ]]; then
  sed -e 's/tcmalloc/aws_lc/g' \
    "${OUTPUT}/summary.txt" >"${OUTPUT}/provider-summary.txt"
  cat "${OUTPUT}/provider-summary.txt"
fi

echo "tls_provider_results=${OUTPUT} exit_code=${rc}"
exit "${rc}"
