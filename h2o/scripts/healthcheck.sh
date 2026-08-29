#!/bin/bash
set -eu

HOST=${H2O_HEALTH_HOST:-health.invalid}
PORT=${H2O_HEALTH_PORT:-80}
PATH_=${H2O_HEALTH_PATH:-/h2o-health}

if ! exec 3<>"/dev/tcp/127.0.0.1/${PORT}"; then
  exit 1
fi

printf 'GET %s HTTP/1.1\r\nHost: %s\r\nConnection: close\r\n\r\n' \
  "${PATH_}" "${HOST}" >&3

while IFS= read -r line <&3; do
  case "${line}" in
    HTTP/*\ 200\ *) exit 0 ;;
  esac
done

exit 1
