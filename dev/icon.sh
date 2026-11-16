#!/usr/bin/env bash
#
# Renders each channel's SVG into every platform's icon: the macOS .icns, the
# Windows .ico and the Linux PNGs. assets/icon/dbdelve.svg is stable's and
# assets/icon/dbdelve-nightly.svg is Nightly's. The outputs are committed so
# building and packaging never need an SVG renderer -- run this after changing
# either SVG.
#
# Needs resvg and ImageMagick: brew install resvg imagemagick
#
# Usage: dev/icon.sh
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

# $1 is the slug the Linux and Windows files are named after, $2 the name of
# the macOS .icns.
icons() {
  local slug="$1" icns="$2"
  local svg="assets/icon/$slug.svg"
  render() { resvg -w "$1" -h "$1" "$svg" "$2"; }

  for SIZE in 16 32 64 128 256 512; do
    render "$SIZE" "assets/linux/icons/$slug-$SIZE.png"
  done

  mkdir "$TMP/$icns.iconset"
  for SIZE in 16 32 128 256 512; do
    render "$SIZE" "$TMP/$icns.iconset/icon_${SIZE}x${SIZE}.png"
    render "$((SIZE * 2))" "$TMP/$icns.iconset/icon_${SIZE}x${SIZE}@2x.png"
  done
  iconutil -c icns "$TMP/$icns.iconset" -o "assets/macos/$icns.icns"

  magick "assets/linux/icons/$slug"-{16,32,64,128,256}.png "assets/windows/$slug.ico"
}

icons dbdelve DBDelve
icons dbdelve-nightly DBDelve-Nightly
