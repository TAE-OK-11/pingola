#!/usr/bin/env bash
# Validate the mounted certificate, key, symlink targets, PEM, key match, and
# static roots as the image's non-root UID before restarting the service.
set -euo pipefail

COMPOSE_FILE=${PINGORA_COMPOSE_FILE:-docker-compose.yml}
SERVICE=${PINGORA_COMPOSE_SERVICE:-pingora}
CONFIG=${PINGORA_CONFIG:-/etc/pingora/pingora.yaml}

echo "validating renewed certificates in compose service ${SERVICE}"
docker compose -f "${COMPOSE_FILE}" run --rm --no-deps \
  --entrypoint /usr/local/bin/pingora \
  "${SERVICE}" --config "${CONFIG}" --check
