#!/usr/bin/env bash
# Scale PGO training workload sizes. Docker publish sets PGO_TRAIN_FAST=on to
# keep CI wall time reasonable while preserving scenario coverage.
pgo_train_scale() {
  local value=$1
  if [[ "${PGO_TRAIN_FAST:-off}" == on ]]; then
    local scaled=$(( (value + 1) / 2 ))
    (( scaled < 1 )) && scaled=1
    printf '%s' "${scaled}"
    return
  fi
  printf '%s' "${value}"
}
