#!/bin/sh
#
# Installs the latest DBDelve release on macOS:
#
#   curl -fsSL https://raw.githubusercontent.com/ShayanAbbas1/dbdelve/main/install.sh | sh
#
# DBDelve is signed ad-hoc and not notarized, so a .dmg downloaded with a
# browser is quarantined and Gatekeeper refuses the app until that is cleared
# by hand. curl quarantines nothing, so an app installed from here just opens.
#
# DBDELVE_INSTALL_DIR picks the folder the app goes in; the default is
# /Applications, or ~/Applications when that is not writable.
set -eu

REPO="ShayanAbbas1/dbdelve"
IDENTIFIER="com.shayanabbas.dbdelve"
# The key src/update/macos.rs trusts; the release workflow fails if the two
# ever differ.
PUBLIC_KEY="RWS7Oo2qbLFxuFuw9Ygz9Ew/UX5XomznTQHGTHGNRfmLqWPa3RqbChNw"

fail() {
  echo "install.sh: $*" >&2
  exit 1
}

# All of it inside a function, called on the last line: a download cut short
# then runs nothing rather than half a script.
main() {
  [ "$(uname -s)" = Darwin ] ||
    fail "this installs the macOS app; see https://github.com/$REPO/blob/main/docs/install.md for other systems"
  [ "$(uname -m)" = arm64 ] || fail "DBDelve is built for Apple Silicon only"

  # The tag comes from where /releases/latest redirects to, which, unlike the
  # API, is not rate limited.
  latest="$(curl -fsSLI -o /dev/null -w '%{url_effective}' "https://github.com/$REPO/releases/latest")"
  version="${latest##*/v}"
  case "$version" in
    '' | *[!0-9.]*) fail "could not work out the latest release from $latest" ;;
  esac

  if [ -n "${DBDELVE_INSTALL_DIR:-}" ]; then
    directory="$DBDELVE_INSTALL_DIR"
  elif [ -w /Applications ]; then
    directory=/Applications
  else
    directory="$HOME/Applications"
  fi
  mkdir -p "$directory"
  app="$directory/DBDelve.app"
  if pgrep -f "$app/Contents/MacOS/" >/dev/null 2>&1; then
    fail "DBDelve is running from $app -- quit it and run this again"
  fi

  scratch="$(mktemp -d)"
  mount="$scratch/mnt"
  staged="$directory/.DBDelve.install.app"
  cleanup() {
    hdiutil detach "$mount" -force >/dev/null 2>&1 || true
    rm -rf "$scratch" "$staged"
  }
  trap cleanup EXIT
  trap 'exit 1' INT TERM

  dmg="$scratch/DBDelve-$version.dmg"
  url="https://github.com/$REPO/releases/download/v$version/DBDelve-$version.dmg"
  echo "Downloading DBDelve $version..."
  curl -fL --progress-bar -o "$dmg" "$url"

  # Stock macOS has nothing that checks a minisign signature, so this is for
  # those who have it; everyone else is trusting GitHub over HTTPS, as they
  # are for this script.
  if command -v minisign >/dev/null 2>&1 && curl -fsL -o "$dmg.minisig" "$url.minisig" 2>/dev/null; then
    minisign -V -q -P "$PUBLIC_KEY" -m "$dmg" -x "$dmg.minisig" ||
      fail "the download's signature did not check out"
    echo "Signature verified."
  fi

  mkdir "$mount"
  hdiutil attach -nobrowse -readonly -noautoopen -mountpoint "$mount" "$dmg" >/dev/null
  [ -d "$mount/DBDelve.app" ] || fail "the download holds no DBDelve.app"
  rm -rf "$staged"
  ditto "$mount/DBDelve.app" "$staged"

  codesign --verify --deep --strict "$staged" || fail "the downloaded app is damaged"
  found="$(plutil -extract CFBundleIdentifier raw -o - "$staged/Contents/Info.plist")"
  [ "$found" = "$IDENTIFIER" ] || fail "the downloaded app is $found, not DBDelve"

  rm -rf "$app"
  mv "$staged" "$app"
  echo "DBDelve $version is in $app"
  echo "Open it from there, or with: open \"$app\""
}

main "$@"
