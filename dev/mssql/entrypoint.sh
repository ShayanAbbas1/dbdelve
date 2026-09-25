#!/bin/bash
set -eu

/opt/mssql/bin/sqlservr &
SQLSERVR_PID=$!

SQLCMD="/opt/mssql-tools18/bin/sqlcmd -C -S localhost -U sa -P $MSSQL_SA_PASSWORD -b"

until $SQLCMD -Q "SELECT 1" > /dev/null 2>&1; do
    sleep 1
done

# The data volume persists across restarts; the marker file is what tells a
# restart not to re-run the seed against an already-seeded database.
MARKER=/var/opt/mssql/data/.dbdelve-seeded
if [ ! -f "$MARKER" ]; then
    $SQLCMD -i /dbdelve-init/001-dbdelve-demo.sql
    touch "$MARKER"
fi

wait "$SQLSERVR_PID"
