#!/usr/bin/env bash
# Downloads IBM's Data Server Driver for ODBC and CLI for this platform into
# dev/db2/.cache (gitignored; nothing licensed by IBM is committed) and prints
# the DRIVER= path for a connection string. It writes an odbcinst.ini and
# odbc.ini beside the driver for a driver manager that reads them
# (`eval "$(dev/db2/driver.sh --env)"` exports ODBCSYSINI and ODBCINI), and
# touches no global file.
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
CACHE="$HERE/.cache"
BASE=https://public.dhe.ibm.com/ibmdl/export/pub/software/data/db2/drivers/odbc_cli

case "$(uname -s)-$(uname -m)" in
  Darwin-arm64)
    archive=macarm64_odbc_cli.tar.gz
    sha256=188adacc6a6efd2381f5315bac16b42f0c719ea8c2fce0a1f7a8950e68f84011
    # The macOS driver is itself the ODBC library; there is no libdb2o.
    library=libdb2.dylib ;;
  Linux-x86_64)
    archive=linuxx64_odbc_cli.tar.gz
    sha256=77b754d19be2cd5c55f6feb8bd951c2b17aa6646289944855612c5a2ac555f55
    # The Linux tarball ships only libdb2.so (and libdb2.so.1); there is no libdb2o.so.
    library=libdb2.so ;;
  *) echo "no IBM ODBC driver is staged for $(uname -s) $(uname -m)" >&2; exit 1 ;;
esac

mkdir -p "$CACHE"
digest() {
  if command -v sha256sum >/dev/null; then sha256sum "$1" | cut -d' ' -f1; else shasum -a 256 "$1" | cut -d' ' -f1; fi
}

if [ ! -f "$CACHE/$archive" ] || [ "$(digest "$CACHE/$archive")" != "$sha256" ]; then
  curl -fsSL -o "$CACHE/$archive" "$BASE/$archive"
  if [ "$(digest "$CACHE/$archive")" != "$sha256" ]; then
    rm -f "$CACHE/$archive"
    echo "$archive does not match the pinned SHA-256; has IBM replaced it? update dev/db2/driver.sh" >&2
    exit 1
  fi
fi

if [ ! -d "$CACHE/clidriver" ]; then
  tar -xzf "$CACHE/$archive" -C "$CACHE"
fi

driver="$CACHE/clidriver/lib/$library"
[ -f "$driver" ] || { echo "$library is not in the archive" >&2; exit 1; }

mkdir -p "$CACHE/odbc"
printf '[Db2]\nDescription=IBM Db2 ODBC and CLI\nDriver=%s\n' "$driver" > "$CACHE/odbc/odbcinst.ini"
: > "$CACHE/odbc/odbc.ini"

if [ "${1:-}" = "--env" ]; then
  echo "export ODBCSYSINI='$CACHE/odbc' ODBCINI='$CACHE/odbc/odbc.ini'"
else
  echo "DRIVER=$driver"
fi
