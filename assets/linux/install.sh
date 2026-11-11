#!/usr/bin/env bash
#
# Installs DBDelve for the current user, out of the unpacked release tarball.
#
# Everything lands under ~/.local, so this never needs root and never fights
# the distribution's package manager over a path it owns.
#
# Usage: ./install.sh [--uninstall]
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BIN="$HOME/.local/bin"
ICONS="$HOME/.local/share/icons/hicolor"
APPS="$HOME/.local/share/applications"
SIZES=(16 32 64 128 256 512)

refresh() {
  # Both are optional: a desktop without them still reads the directories on
  # its own schedule, and a headless box has neither installed.
  if command -v update-desktop-database >/dev/null; then
    update-desktop-database "$APPS" || true
  fi
  if command -v gtk-update-icon-cache >/dev/null; then
    gtk-update-icon-cache -qtf "$ICONS" || true
  fi
}

if [[ "${1:-}" == --uninstall ]]; then
  rm -f "$BIN/dbdelve" "$APPS/dbdelve.desktop"
  rm -rf "${XDG_DATA_HOME:-$HOME/.local/share}/dbdelve/plugins/odbc"
  for size in "${SIZES[@]}"; do
    rm -f "$ICONS/${size}x${size}/apps/dbdelve.png"
  done
  refresh
  # Connections, query history and saved passwords are deliberately left: an
  # uninstall here is usually a reinstall, and they live outside all of this.
  echo "removed dbdelve -- data in ~/.local/share/dbdelve was left alone"
  exit 0
fi

mkdir -p "$BIN" "$APPS"
# Beside it and then renamed over it, so the path never holds a half-written
# binary, and a running DBDelve -- the one updating itself, say -- keeps the
# file it started from rather than having it rewritten under it.
install -m 755 "$HERE/dbdelve" "$BIN/.dbdelve.new"
mv -f "$BIN/.dbdelve.new" "$BIN/dbdelve"
# The binary lands alone in ~/.local/bin, so its bridge goes to the per-user
# plugin directory db::plugin::discover() also reads, not beside the binary.
PLUGINS="${XDG_DATA_HOME:-$HOME/.local/share}/dbdelve/plugins"
rm -rf "$PLUGINS/odbc"
mkdir -p "$PLUGINS"
cp -R "$HERE/plugins/odbc" "$PLUGINS/odbc"
# Exec gets the absolute path rather than the bare name the entry ships with:
# a desktop launches its entries with the session's PATH, and ~/.local/bin only
# joins that at login -- and only when it already existed. Bare `dbdelve` would
# leave the icon there and do nothing when clicked, until the next login.
sed "s|^Exec=dbdelve |Exec=$BIN/dbdelve |" "$HERE/dbdelve.desktop" > "$APPS/dbdelve.desktop"
chmod 644 "$APPS/dbdelve.desktop"
for size in "${SIZES[@]}"; do
  mkdir -p "$ICONS/${size}x${size}/apps"
  install -m 644 "$HERE/icons/dbdelve-$size.png" "$ICONS/${size}x${size}/apps/dbdelve.png"
done
refresh

echo "installed $BIN/dbdelve"
case ":$PATH:" in
  *":$BIN:"*) ;;
  *) echo "note: $BIN is not on your PATH, so running dbdelve from a terminal needs a new login -- the menu entry works now regardless" >&2 ;;
esac
