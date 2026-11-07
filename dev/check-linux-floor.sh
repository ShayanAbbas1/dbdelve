#!/usr/bin/env bash
#
# Fails if a Linux binary needs a newer glibc, libstdc++ or libgcc_s than
# Ubuntu 22.04 ships. None of the three travel with the release -- the tarball
# and the AppImage both take them from the host -- so the oldest system the
# release runs on is set by the symbol versions the linker recorded, and those
# follow whatever toolchain the build runner happens to have. A runner image
# that picks up a newer libstdc++ would otherwise ship a binary that no longer
# starts on 22.04 or Debian 12, with nothing failing here.
#
# The floor is stated rather than read off the build host, since drift on the
# build host is the thing being caught. The values are the newest versions
# each library carries on a stock, updated Ubuntu 22.04.
#
# Usage: dev/check-linux-floor.sh BINARY...
set -euo pipefail

case "$(uname -m)" in
  x86_64) GCC_FLOOR=12.0.0 ;;
  aarch64) GCC_FLOOR=11.0 ;;
  *) echo "no floor recorded for $(uname -m)" >&2; exit 1 ;;
esac
FLOORS=(GLIBC_2.35 GLIBCXX_3.4.30 CXXABI_1.3.13 "GCC_$GCC_FLOOR")

FAILED=0
for BIN in "$@"; do
  NEEDED="$(objdump -p "$BIN" | awk '/^ +0x[0-9a-f]+ 0x[0-9a-f]+ [0-9]+ /{print $4}' | sort -u)"
  for FLOOR in "${FLOORS[@]}"; do
    FAMILY="${FLOOR%_*}"
    # The family's own prefix and an underscore, so GLIBC_ does not also
    # match GLIBCXX_. A non-numeric suffix such as GLIBC_ABI_DT_RELR sorts
    # above every number and fails, which is right: it needs glibc 2.36.
    NEWEST="$(grep "^${FAMILY}_" <<<"$NEEDED" | sort -V | tail -1 || true)"
    [[ -z "$NEWEST" ]] && continue
    if [[ "$(printf '%s\n%s\n' "$FLOOR" "$NEWEST" | sort -V | tail -1)" != "$FLOOR" ]]; then
      echo "$BIN needs $NEWEST, newer than Ubuntu 22.04's $FLOOR" >&2
      FAILED=1
    fi
  done
done
exit "$FAILED"
