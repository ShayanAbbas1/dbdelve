#!/bin/bash
set -eu

/opt/mssql/bin/sqlservr &
SQLSERVR_PID=$!

SQLCMD="/opt/mssql-tools18/bin/sqlcmd -C -S localhost -U sa -P $MSSQL_SA_PASSWORD -b"

until $SQLCMD -Q "SELECT 1" > /dev/null 2>&1; do
    sleep 1
done

# The data volume persists across restarts; the marker file is what tells a
# restart not to re-run the seed against an already-seeded database. It is
# written only once the seed succeeds, so a seed that failed part way left its
# objects behind unmarked: drop them first, or every later start fails on them.
MARKER=/var/opt/mssql/data/.dbdelve-seeded
if [ ! -f "$MARKER" ]; then
    $SQLCMD -Q "IF DB_ID('dbdelve_dev') IS NOT NULL BEGIN
                    ALTER DATABASE dbdelve_dev SET SINGLE_USER WITH ROLLBACK IMMEDIATE;
                    DROP DATABASE dbdelve_dev;
                END;
                IF SUSER_ID('dbdelve') IS NOT NULL DROP LOGIN dbdelve;"
    $SQLCMD -i /dbdelve-init/001-dbdelve-demo.sql
    touch "$MARKER"
fi

wait "$SQLSERVR_PID"
