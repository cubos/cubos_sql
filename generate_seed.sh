#!/usr/bin/env bash
#
# Generates cubos_sql_analyzer/src/seed.json from a clean PostgreSQL instance.
#
# This script:
#   1. Starts a temporary postgres:18 Docker container
#   2. Waits for it to be ready
#   3. Runs the generate_seed example to export pg_catalog
#   4. Stops and removes the container
#
# Usage:
#   ./generate_seed.sh              # uses postgres:18 (default)
#   ./generate_seed.sh postgres:17  # use a specific image
#
# The seed.json file is written to cubos_sql_analyzer/src/seed.json
# and should be committed to the repository.

set -euo pipefail

PG_IMAGE="${1:-postgres:18}"
CONTAINER_NAME="cubos_sql_seed_$$"

cd "$(dirname "$0")"

echo "==> Starting ${PG_IMAGE} container..."
CONTAINER_ID=$(docker run -d --rm \
    --name "$CONTAINER_NAME" \
    -e POSTGRES_PASSWORD=postgres \
    -p 0:5432 \
    "$PG_IMAGE")

cleanup() {
    echo "==> Cleaning up container ${CONTAINER_ID:0:12}..."
    docker rm -f "$CONTAINER_ID" >/dev/null 2>&1 || true
}
trap cleanup EXIT

# Get the mapped port
PORT=$(docker port "$CONTAINER_ID" 5432 | head -1 | sed 's/.*://')
echo "==> Container ${CONTAINER_ID:0:12} running on port ${PORT}"

# Wait for postgres to be ready
echo "==> Waiting for PostgreSQL to accept connections..."
for i in $(seq 1 30); do
    if docker exec "$CONTAINER_ID" pg_isready -U postgres -d postgres >/dev/null 2>&1; then
        break
    fi
    if [ "$i" -eq 30 ]; then
        echo "ERROR: PostgreSQL did not become ready in 30 seconds"
        exit 1
    fi
    sleep 1
done

echo "==> Generating seed.json..."
CONN="host=127.0.0.1 port=${PORT} user=postgres password=postgres dbname=postgres"
cargo run -p cubos_sql_seed -- "$CONN"

echo "==> Done! seed.json has been updated."
echo "    Remember to commit cubos_sql_analyzer/src/seed.json"
