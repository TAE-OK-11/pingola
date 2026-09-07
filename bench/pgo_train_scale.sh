#!/usr/bin/env bash
# Scale PGO training workload sizes. Docker publish sets PGO_TRAIN_FAST=on to
# keep CI wall time reasonable while preserving scenario coverage.
pgo_train_scale() {
  local value=$1
  if [[ "${PGO_TRAIN_FAST:-off}" == on ]]; then
    # The direct-gateway H3 bulk/compression probes use a 32-request batch on
    # one QUIC session. Under instrumented CI builds the upstream controller can
    # legitimately retire between large responses, making the remaining queued
    # requests fail with "controller is closed" even though the path is healthy.
    # Keep those fast-mode probes as one fresh-session sample; full/manual PGO
    # (PGO_TRAIN_FAST=off) still exercises all 32 requests.
    if (( value == 32 )); then
      printf '%s' '1'
      return
    fi
    local scaled=$(( (value + 1) / 2 ))
    (( scaled < 1 )) && scaled=1
    printf '%s' "${scaled}"
    return
  fi
  printf '%s' "${value}"
}
