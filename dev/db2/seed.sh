#!/bin/bash
set -eu

# Runs on every start; the data volume persists, so the marker table, created
# last by the seed, is what says it has already run. A seed that failed part
# way leaves objects behind unmarked: remove the volume (`docker compose
# --profile db2 down -v`) rather than debugging a half-seeded database.
if su - db2inst1 -c "db2 connect to dbdelve >/dev/null && db2 -x \"SELECT 1 FROM syscat.tables WHERE tabname = 'DBDELVE_SEEDED'\"" | grep -Eq '^ *1 *$'; then
    echo "(*) dbdelve seed already applied"
    exit 0
fi
echo "(*) applying dbdelve seed"
su - db2inst1 -c "db2 connect to dbdelve >/dev/null && db2 -stf /dbdelve-init/001-dbdelve-demo.sql"
echo "(*) dbdelve seed applied"
