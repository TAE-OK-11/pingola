#!/usr/bin/env bash
set -euo pipefail

LTO_MODE=${LTO_MODE:-fat}

case "${LTO_MODE}" in
  fat) printf '%s' '-flto' ;;
  thin) printf '%s' '-flto=thin' ;;
  off) printf '%s' '' ;;
  *)
    echo "unsupported LTO mode: ${LTO_MODE}" >&2
    exit 2
    ;;
esac
