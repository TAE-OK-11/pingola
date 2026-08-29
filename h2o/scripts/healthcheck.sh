#!/bin/sh
set -eu

HOST=${H2O_HEALTH_HOST:-health.invalid}
PORT=${H2O_HEALTH_PORT:-80}
PATH_=${H2O_HEALTH_PATH:-/h2o-health}

exec curl --fail --silent --show-error --max-time 2 \
  --header "Host: ${HOST}" \
  "http://127.0.0.1:${PORT}${PATH_}" \
  >/dev/null
