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
# The binary lands alone in ~/.local/bin, so its bridge goes to the per-user
# plugin directory db::plugin::discover() also reads, not beside the binary.
DATA="${XDG_DATA_HOME:-$HOME/.local/share}/dbdelve"
PLUGINS="$DATA/plugins"
# The bridge's staged and backup copies: beside plugins/ rather than in it,
# where another DBDelve would load them, and on its filesystem, so moving one
# into place is a rename.
STAGED="$DATA/.odbc.new"
BACKUP="$DATA/.odbc.old"
SIZES=(16 32 64 128 256 512)

# One install or uninstall at a time: two would share the staged and backup
# names above and undo each other's work. A lock file of this script's own --
# the app's updater, which runs this, already holds one on ~/.local/bin.
mkdir -p "$DATA"
if command -v flock >/dev/null; then
  exec 9>"$DATA/.install.lock"
  flock -n 9 || {
    echo "another dbdelve install is running" >&2
    exit 1
  }
fi

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
  rm -f "$BIN/dbdelve" "$BIN/.dbdelve.new" "$APPS/dbdelve.desktop"
  rm -rf "$PLUGINS/odbc" "$STAGED" "$BACKUP"
  for size in "${SIZES[@]}"; do
    rm -f "$ICONS/${size}x${size}/apps/dbdelve.png"
  done
  refresh
  # Connections, query history and saved passwords are deliberately left: an
  # uninstall here is usually a reinstall, and they live outside all of this.
  echo "removed dbdelve -- data in ~/.local/share/dbdelve was left alone"
  exit 0
fi

mkdir -p "$BIN" "$APPS" "$PLUGINS"

# A run killed during its commit, below, left the backup. With the new binary
# still staged, or no bridge in place, the commit had not finished and the
# backup is the installed bridge; otherwise it had, and the backup is spare.
if [[ -d "$BACKUP" ]]; then
  if [[ -e "$BIN/.dbdelve.new" || ! -e "$PLUGINS/odbc" ]]; then
    rm -rf "$PLUGINS/odbc"
    mv "$BACKUP" "$PLUGINS/odbc"
  else
    rm -rf "$BACKUP"
  fi
fi

# The binary and its bridge are copied beside where they go, then renamed into
# place only once both copies are whole, and a failure part way through the
# renames puts the old bridge back. A running DBDelve -- the one updating
# itself, say -- keeps the files it started from.
moved_aside=
swapped=
committed=
roll_back() {
  if [[ -z "$committed" ]]; then
    if [[ -n "$swapped" ]]; then
      rm -rf "$PLUGINS/odbc"
    fi
    if [[ -n "$moved_aside" && ! -e "$PLUGINS/odbc" ]]; then
      mv "$BACKUP" "$PLUGINS/odbc"
    fi
  fi
  rm -rf "$BIN/.dbdelve.new" "$STAGED"
}
trap roll_back EXIT
trap 'exit 1' HUP INT TERM
rm -rf "$BIN/.dbdelve.new" "$STAGED"
install -m 755 "$HERE/dbdelve" "$BIN/.dbdelve.new"
cp -R "$HERE/plugins/odbc" "$STAGED"
# Ignored while only renames run, so a signal cannot land between a rename and
# the flag that records it and leave roll_back undoing the wrong step.
trap '' HUP INT TERM
if [[ -e "$PLUGINS/odbc" || -L "$PLUGINS/odbc" ]]; then
  mv "$PLUGINS/odbc" "$BACKUP"
  moved_aside=1
fi
mv "$STAGED" "$PLUGINS/odbc"
swapped=1
mv -f "$BIN/.dbdelve.new" "$BIN/dbdelve"
committed=1
rm -rf "$BACKUP"
trap 'exit 1' HUP INT TERM
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
