#!/usr/bin/env bash
#
# Builds DBDelve for Linux and wraps it in the release tarball: the binary, its
# icons, a desktop entry and an install.sh that puts them where a desktop looks.
#
# Nothing is signed and nothing is installed from here -- the tarball is what
# ships, and unpacking it is the user's half of the job.
#
# Usage: dev/package-linux.sh
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

VERSION="$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)"
# Honoured rather than assumed: a build that set CARGO_TARGET_DIR puts the
# binary somewhere else entirely, and reading `target/` regardless is how a
# stale one from an earlier build ends up in the tarball.
TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"
ARCH="$(uname -m)"
NAME="dbdelve-$VERSION-linux-$ARCH"
STAGE="$TARGET_DIR/linux/$NAME"
TARBALL="$TARGET_DIR/linux/$NAME.tar.gz"

cargo build --release
# The ODBC bridge links the system's libodbc.so.2, which is never bundled: a
# user needs the unixodbc package to connect through it, and the app itself
# does not link ODBC.
dev/plugins.sh --release

rm -rf "$STAGE"
mkdir -p "$STAGE/icons"
install -m 755 "$TARGET_DIR/release/dbdelve" "$STAGE/dbdelve"
# Beside the binary, where db::plugin::discover() looks. Only odbc: the staging
# directory also holds echo after a `dev/plugins.sh --echo`, which is test code.
mkdir -p "$STAGE/plugins"
cp -R "$TARGET_DIR/release/plugins/odbc" "$STAGE/plugins/odbc"
install -m 755 assets/linux/install.sh "$STAGE/install.sh"
install -m 644 assets/linux/dbdelve.desktop "$STAGE/dbdelve.desktop"
install -m 644 assets/linux/icons/dbdelve-*.png "$STAGE/icons/"
cp LICENSE NOTICES.md "$STAGE/"
# NOTICES.md points at licenses/OFL-1.1.txt, and the OFL asks that the licence
# travel with the fonts -- which are compiled into the binary above.
cp -R licenses "$STAGE/"

# -C so the archive holds one top-level directory named after the release and
# nothing of the build tree's path.
tar -czf "$TARBALL" -C "$TARGET_DIR/linux" "$NAME"

if command -v sha256sum >/dev/null; then
  SHA="$(sha256sum "$TARBALL" | cut -d' ' -f1)"
else
  SHA="$(shasum -a 256 "$TARBALL" | cut -d' ' -f1)"
fi

echo "built $TARBALL (v$VERSION)"
echo "sha256 $SHA"
