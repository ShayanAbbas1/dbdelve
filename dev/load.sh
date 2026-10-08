#!/usr/bin/env bash
# Rebuilds `events` wide (dev/load/) in the dev databases already running, to
# load-test the grid against. Opt-in only: neither the seeds nor CI run it.
#
#   dev/load.sh                      every engine, a million rows
#   ROWS=100000 dev/load.sh mysql    just MySQL, fewer rows
#
# COMPOSE_PROJECT_NAME picks a stack other than the default; SQLITE_DB a file
# other than dev/dbdelve_dev.db.
set -euo pipefail

cd "$(dirname "$0")/.."
rows=${ROWS:-1000000}
sqlite_db=${SQLITE_DB:-dev/dbdelve_dev.db}
engines=("$@")
[[ ${#engines[@]} -gt 0 ]] || engines=(postgres mysql mariadb mssql sqlite mongo)

# The MySQL and SQL Server scripts count with seven self-joined digits.
if [[ ! $rows =~ ^[1-9][0-9]*$ ]] || ((rows > 10000000)); then
    echo "ROWS must be between 1 and 10000000" >&2
    exit 1
fi

script() { sed "s/@ROWS@/$rows/g" "dev/load/$1"; }

for engine in "${engines[@]}"; do
    echo "$engine: loading $rows rows"
    start=$SECONDS
    case $engine in
        postgres)
            script postgres.sql | docker compose exec -T postgres \
                psql -q -v ON_ERROR_STOP=1 -U dbdelve -d dbdelve_dev ;;
        mysql | mariadb)
            client=mysql
            [[ $engine == mariadb ]] && client=mariadb
            script mysql.sql | docker compose exec -T -e MYSQL_PWD=dbdelve "$engine" \
                "$client" -udbdelve dbdelve_dev ;;
        mssql)
            script mssql.sql | docker compose exec -T mssql \
                /opt/mssql-tools18/bin/sqlcmd -C -b -S localhost -U dbdelve -P DBDelve_dev1 -d dbdelve_dev ;;
        sqlite)
            [[ -f $sqlite_db ]] || { echo "$sqlite_db not found; build it from dev/sqlite first" >&2; exit 1; }
            script sqlite.sql | sqlite3 -bail "$sqlite_db" ;;
        mongo)
            docker compose exec -T mongo \
                mongosh --quiet -u dbdelve -p dbdelve dbdelve_dev --eval "$(script mongo.js)" ;;
        *)
            echo "unknown engine: $engine" >&2
            exit 1 ;;
    esac
    echo "$engine: done in $((SECONDS - start))s"
done
