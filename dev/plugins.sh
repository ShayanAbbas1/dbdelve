#!/usr/bin/env bash
# Builds the plugin crates and stages them where `db::plugin::discover()` looks
# under `cargo run`: target/<profile>/plugins/<id>/{dbdelve-plugin.toml,bin/<exe>}.
# Usage: dev/plugins.sh [--release] [--echo]
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

profile=debug
cargo_flags=()
plugins=(odbc)
for arg in "$@"; do
  case "$arg" in
    --release) profile=release; cargo_flags=(--release) ;;
    # Test code, never shipped: the fake engine the host's tests drive.
    --echo) plugins+=(echo) ;;
    *) echo "usage: dev/plugins.sh [--release] [--echo]" >&2; exit 2 ;;
  esac
done

stage="$ROOT/target/$profile/plugins"

# Before the build: overwriting the executable of a running bridge breaks it.
if pgrep -f "$stage/" >/dev/null; then
  echo "a plugin from $stage is running -- quit the app and re-run" >&2
  exit 1
fi

packages=()
for id in "${plugins[@]}"; do
  packages+=(-p "dbdelve-plugin-$id")
done

if ! cargo build ${cargo_flags[@]+"${cargo_flags[@]}"} "${packages[@]}"; then
  echo "if the link failed on -lodbc: install unixODBC's development files (apt install unixodbc-dev)" >&2
  exit 1
fi

for id in "${plugins[@]}"; do
  dest="$stage/$id"
  exe="$(sed -n 's/^executable *= *"\(.*\)"/\1/p' "plugins/$id/dbdelve-plugin.toml")"
  rm -rf "$dest"
  mkdir -p "$dest/$(dirname "$exe")"
  cp "plugins/$id/dbdelve-plugin.toml" "$dest/"
  cp "target/$profile/$(basename "$exe")" "$dest/$exe"
  echo "staged $dest"
done
