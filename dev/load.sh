#!/usr/bin/env bash
# Rebuilds `events` wide (dev/load/postgres.sql) in the running dev Postgres, to
# load-test the grid against. Opt-in only: neither the seed nor CI run it.
#
#   dev/load.sh                 a million rows
#   ROWS=100000 dev/load.sh     fewer rows
#
# COMPOSE_PROJECT_NAME picks a stack other than the default.
set -euo pipefail

cd "$(dirname "$0")/.."
rows=${ROWS:-1000000}
if [[ ! $rows =~ ^[1-9][0-9]*$ ]]; then
    echo "ROWS must be a positive integer" >&2
    exit 1
fi

start=$SECONDS
sed "s/@ROWS@/$rows/g" dev/load/postgres.sql | docker compose exec -T postgres \
    psql -q -v ON_ERROR_STOP=1 -U dbdelve -d dbdelve_dev
echo "loaded $rows rows in $((SECONDS - start))s"
