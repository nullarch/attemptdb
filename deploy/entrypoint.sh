#!/bin/sh
# Start attemptdb-server against the /data volume.
#
#   /data/keys.json          key digests (never keys); created empty if absent
#   /data/tenants/<tenant>/  one .attemptdb per tenant
#
# ATTEMPTDB_ADMIN_TOKEN enables /v1/admin/* (key issuance, device removal).
#
# ATTEMPTDB_MAX_OPEN caps how many tenant databases stay resident. It is the
# only bound on the server's memory: an open tenant's read cache holds that
# tenant's whole projected history, so RSS is roughly
#   11 MiB + sum over open tenants of (events x ~4-5 KiB).
# Lower it on a small machine; ATTEMPTDB_IDLE_FLUSH_SECS closes idle tenants
# sooner and gives the memory back.
# Extra arguments are passed through to attemptdb-server.
set -eu
DATA_DIR="${ATTEMPTDB_DATA_DIR:-/data}"
KEYS="${ATTEMPTDB_KEYS_FILE:-$DATA_DIR/keys.json}"
if [ ! -f "$KEYS" ]; then
    printf '{"keys":[]}\n' > "$KEYS"
    chmod 0600 "$KEYS"
    echo "created empty key file at $KEYS" >&2
fi
exec attemptdb-server \
    --bind "${ATTEMPTDB_BIND:-0.0.0.0}" \
    --port "${ATTEMPTDB_PORT:-8787}" \
    --data-dir "$DATA_DIR" \
    --keys "$KEYS" \
    --capture-mode "${ATTEMPTDB_CAPTURE_MODE:-metadata_only}" \
    --max-open "${ATTEMPTDB_MAX_OPEN:-256}" \
    --idle-flush-secs "${ATTEMPTDB_IDLE_FLUSH_SECS:-300}" \
    "$@"
