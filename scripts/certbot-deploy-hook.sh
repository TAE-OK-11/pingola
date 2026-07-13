#!/usr/bin/env bash
# Certbot deploy hook. Install and run this as root; it grants only UID 10001
# read/traverse ACLs on one certificate lineage, validates inside the container,
# and restarts Pingora only after validation succeeds.
set -euo pipefail

: "${RENEWED_LINEAGE:?Certbot must set RENEWED_LINEAGE}"
PINGORA_UID=${PINGORA_UID:-10001}
COMPOSE_FILE=${PINGORA_COMPOSE_FILE:-/opt/pingora/docker-compose.yml}
SERVICE=${PINGORA_COMPOSE_SERVICE:-pingora}
CONFIG=${PINGORA_CONFIG:-/etc/pingora/pingora.yaml}

command -v setfacl >/dev/null || {
  echo "setfacl is required (install the acl package)" >&2
  exit 1
}

lineage=$(readlink -f "${RENEWED_LINEAGE}")
archive_root=$(readlink -f "${lineage}/../../archive")
lineage_name=$(basename "${lineage}")
archive_lineage=${archive_root}/${lineage_name}

[[ -d "${archive_lineage}" ]] || {
  echo "archive lineage not found: ${archive_lineage}" >&2
  exit 1
}

# Parent directories need traverse permission; the live symlinks themselves do
# not grant access to archive targets.
current=${lineage}
while [[ "${current}" != / ]]; do
  setfacl -m "u:${PINGORA_UID}:--x" "${current}"
  current=$(dirname "${current}")
done

current=${archive_lineage}
while [[ "${current}" != / ]]; do
  setfacl -m "u:${PINGORA_UID}:--x" "${current}"
  current=$(dirname "${current}")
done

# Existing archive files get read access; the default ACL gives future
# privkeyN.pem files the same access after the next Certbot renewal.
setfacl -R -m "u:${PINGORA_UID}:r-X" "${archive_lineage}"
setfacl -m "d:u:${PINGORA_UID}:r-X" "${archive_lineage}"

certificate_target=$(readlink -f "${RENEWED_LINEAGE}/fullchain.pem")
private_key_target=$(readlink -f "${RENEWED_LINEAGE}/privkey.pem")
echo "renewed certificate target: ${certificate_target}"
echo "renewed private key target: ${private_key_target} (contents are never printed)"

docker compose -f "${COMPOSE_FILE}" run --rm --no-deps \
  --entrypoint /usr/local/bin/pingora \
  "${SERVICE}" --config "${CONFIG}" --check

docker compose -f "${COMPOSE_FILE}" restart "${SERVICE}"
echo "Pingora certificate validation and restart completed"
