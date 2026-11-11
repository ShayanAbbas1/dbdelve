#!/bin/sh
#
# Installs the latest DBDelve release on macOS or Linux:
#
#   curl -fsSL https://raw.githubusercontent.com/ShayanAbbas1/dbdelve/main/install.sh | sh
#
# DBDelve is signed ad-hoc and not notarized, so a .dmg downloaded with a
# browser is quarantined and Gatekeeper refuses the app until that is cleared
# by hand. curl quarantines nothing, so an app installed from here just opens.
#
# On Linux it unpacks the release tarball and runs the install.sh inside it,
# which puts everything under ~/.local and needs no root.
#
# On macOS, DBDELVE_INSTALL_DIR picks the folder the app goes in; the default
# is /Applications, or ~/Applications when that is not writable.
set -eu

REPO="ShayanAbbas1/dbdelve"
IDENTIFIER="com.shayanabbas.dbdelve"
# The key src/update/download.rs trusts; the release workflow fails if the two
# ever differ.
PUBLIC_KEY="RWS7Oo2qbLFxuFuw9Ygz9Ew/UX5XomznTQHGTHGNRfmLqWPa3RqbChNw"

fail() {
  echo "install.sh: $*" >&2
  exit 1
}

# Downloads $1 to $2 and, where minisign can check it, verifies it. Stock
# macOS has nothing that checks a minisign signature, so this is for those who
# have it; everyone else is trusting GitHub over HTTPS, as they are for this
# script. Where it can be checked it must be: a signature that will not
# download is a failure, not a reason to skip the check.
download() {
  echo "Downloading DBDelve $version..."
  curl -fL --progress-bar -o "$2" "$1"

  if command -v minisign >/dev/null 2>&1; then
    curl -fsL -o "$2.minisig" "$1.minisig" 2>/dev/null ||
      fail "the signature of DBDelve $version could not be downloaded, so the download cannot be checked"
    minisign -V -q -P "$PUBLIC_KEY" -m "$2" -x "$2.minisig" ||
      fail "the download's signature did not check out"
    echo "Signature verified."
  fi
}

install_macos() {
  [ "$(uname -m)" = arm64 ] || fail "DBDelve is built for Apple Silicon only"

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
  # Beside the destination, so putting the app in place is a rename, and a
  # folder of this run's own, so two runs cannot remove each other's copy.
  stage="$(mktemp -d "$directory/.DBDelve.install.XXXXXX")"
  staged="$stage/DBDelve.app"
  previous="$stage/previous.app"
  cleanup() {
    # Interrupted between moving the old app out and the new one in: the old
    # one goes back rather than leaving nothing installed.
    if [ ! -e "$app" ] && [ -d "$previous" ]; then
      mv "$previous" "$app" || echo "install.sh: the app that was installed is at $previous" >&2
    fi
    hdiutil detach "$mount" -force >/dev/null 2>&1 || true
    rm -rf "$scratch"
    # Unless the old app is in there and could not go back.
    if [ -e "$app" ] || [ ! -d "$previous" ]; then
      rm -rf "$stage"
    fi
  }
  trap cleanup EXIT
  trap 'exit 1' INT TERM

  dmg="$scratch/DBDelve-$version.dmg"
  download "https://github.com/$REPO/releases/download/v$version/DBDelve-$version.dmg" "$dmg"

  mkdir "$mount"
  hdiutil attach -nobrowse -readonly -noautoopen -mountpoint "$mount" "$dmg" >/dev/null
  [ -d "$mount/DBDelve.app" ] || fail "the download holds no DBDelve.app"
  ditto "$mount/DBDelve.app" "$staged"

  codesign --verify --deep --strict "$staged" || fail "the downloaded app is damaged"
  found="$(plutil -extract CFBundleIdentifier raw -o - "$staged/Contents/Info.plist")"
  [ "$found" = "$IDENTIFIER" ] || fail "the downloaded app is $found, not DBDelve"

  # The installed app is moved aside, not deleted, until the new one is in
  # its place; cleanup puts it back if that never happens.
  if [ -e "$app" ]; then
    mv "$app" "$previous"
  fi
  mv "$staged" "$app"
  echo "DBDelve $version is in $app"
  echo "Open it from there, or with: open \"$app\""
}

install_linux() {
  case "$(uname -m)" in
    x86_64 | amd64) arch=x86_64 ;;
    aarch64 | arm64) arch=aarch64 ;;
    *) fail "DBDelve is built for x86_64 and aarch64 on Linux, not $(uname -m)" ;;
  esac
  command -v tar >/dev/null 2>&1 || fail "tar is needed to unpack the release and was not found"
  command -v bash >/dev/null 2>&1 || fail "bash is needed to run the release's installer and was not found"

  scratch="$(mktemp -d)"
  trap 'rm -rf "$scratch"' EXIT
  trap 'exit 1' INT TERM

  name="dbdelve-$version-linux-$arch"
  tarball="$scratch/$name.tar.gz"
  download "https://github.com/$REPO/releases/download/v$version/$name.tar.gz" "$tarball"

  tar -xzf "$tarball" -C "$scratch"
  [ -f "$scratch/$name/install.sh" ] || fail "the download holds no $name/install.sh"
  bash "$scratch/$name/install.sh"
}

# All of it inside a function, called on the last line: a download cut short
# then runs nothing rather than half a script.
main() {
  os="$(uname -s)"
  case "$os" in
    Darwin | Linux) ;;
    *) fail "this installs DBDelve on macOS and Linux; see https://github.com/$REPO/blob/main/docs/install.md for other systems" ;;
  esac

  # The tag comes from where /releases/latest redirects to, which, unlike the
  # API, is not rate limited.
  latest="$(curl -fsSLI -o /dev/null -w '%{url_effective}' "https://github.com/$REPO/releases/latest")"
  version="${latest##*/v}"
  case "$version" in
    '' | *[!0-9.]*) fail "could not work out the latest release from $latest" ;;
  esac

  if [ "$os" = Darwin ]; then
    install_macos
  else
    install_linux
  fi
}

main "$@"
