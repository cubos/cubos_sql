#!/usr/bin/env bash
#
# Spin up a real PostgreSQL Docker container, export POSTGRES_URL pointing
# at it, run `cargo nextest run --features pg_sanity` (passing through any
# extra args), then tear the container down — even on failure.
#
# Each `PgCatalog::new()` in the analyzer's tests will create its own
# scratch database inside the cluster (and DROP it on Drop). All tests
# share one PG cluster, so initdb / pg_ctl runs once total.
#
# Usage:
#   scripts/run-pg-sanity.sh                 # run the whole analyzer suite
#   scripts/run-pg-sanity.sh -p pgsafe_analyzer alter_column   # filter

set -euo pipefail

PG_IMAGE="${PG_IMAGE:-postgres:18}"
CONTAINER_NAME="pgsafe-pg-sanity-$$"
PG_USER="postgres"
PG_PASS="postgres"
PG_DB="postgres"

cleanup() {
    # Stop wins over rm so we exit fast on Ctrl-C; --rm on the container
    # handles the actual deletion. Errors during cleanup are swallowed —
    # the container may already be gone if the test process exited.
    docker rm -f "$CONTAINER_NAME" >/dev/null 2>&1 || true
}
trap cleanup EXIT INT TERM

echo "pg_sanity: starting $PG_IMAGE as $CONTAINER_NAME..."
docker run -d --rm \
    --name "$CONTAINER_NAME" \
    -e POSTGRES_USER="$PG_USER" \
    -e POSTGRES_PASSWORD="$PG_PASS" \
    -e POSTGRES_DB="$PG_DB" \
    -p 127.0.0.1:0:5432 \
    "$PG_IMAGE" >/dev/null

# Discover the host port the daemon picked. `docker port` reports
# `0.0.0.0:NNNNN` (and on macOS often a v6 line too); take the first
# numeric tail.
PG_PORT=$(docker port "$CONTAINER_NAME" 5432 | head -n 1 | awk -F: '{print $NF}')
if [[ -z "$PG_PORT" ]]; then
    echo "pg_sanity: could not determine host port for $CONTAINER_NAME" >&2
    exit 1
fi

# Wait until the cluster accepts connections. `pg_isready` runs inside the
# container so we don't need a libpq client on the host.
echo "pg_sanity: waiting for PG to accept connections on 127.0.0.1:$PG_PORT..."
for _ in $(seq 1 60); do
    if docker exec "$CONTAINER_NAME" pg_isready -U "$PG_USER" -d "$PG_DB" >/dev/null 2>&1; then
        break
    fi
    sleep 0.5
done
if ! docker exec "$CONTAINER_NAME" pg_isready -U "$PG_USER" -d "$PG_DB" >/dev/null 2>&1; then
    echo "pg_sanity: PG never became ready; container logs:" >&2
    docker logs "$CONTAINER_NAME" >&2 || true
    exit 1
fi

export POSTGRES_URL="host=127.0.0.1 port=$PG_PORT user=$PG_USER password=$PG_PASS dbname=$PG_DB"
echo "pg_sanity: POSTGRES_URL=$POSTGRES_URL"
echo "pg_sanity: running tests..."

# Default to the analyzer suite; let callers override / append. Don't
# forward `--release` automatically — leave that to the caller (or to
# nextest's own profile-driven defaults).
if [[ $# -eq 0 ]]; then
    set -- --release --features pg_sanity -p pgsafe_analyzer
fi

cargo nextest run "$@"
