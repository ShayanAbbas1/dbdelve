#!/usr/bin/env bash
#
# Renders assets/icon/dbdelve.svg into every platform's icon: the macOS .icns,
# the Windows .ico and the Linux PNGs. The outputs are committed so building
# and packaging never need an SVG renderer -- run this after changing the SVG.
#
# Needs resvg and ImageMagick: brew install resvg imagemagick
#
# Usage: dev/icon.sh
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

SVG=assets/icon/dbdelve.svg
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

render() { resvg -w "$1" -h "$1" "$SVG" "$2"; }

for SIZE in 16 32 64 128 256 512; do
  render "$SIZE" "assets/linux/icons/dbdelve-$SIZE.png"
done

mkdir "$TMP/DBDelve.iconset"
for SIZE in 16 32 128 256 512; do
  render "$SIZE" "$TMP/DBDelve.iconset/icon_${SIZE}x${SIZE}.png"
  render "$((SIZE * 2))" "$TMP/DBDelve.iconset/icon_${SIZE}x${SIZE}@2x.png"
done
iconutil -c icns "$TMP/DBDelve.iconset" -o assets/macos/DBDelve.icns

magick assets/linux/icons/dbdelve-{16,32,64,128,256}.png assets/windows/dbdelve.ico
