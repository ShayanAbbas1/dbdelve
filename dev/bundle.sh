#!/usr/bin/env bash
#
# Builds DBDelve as a macOS .app.
#
# Dev by default: its own bundle id, its own name, its own profiles and its own
# Keychain items, run from the build tree and never installed, so a build can
# neither replace the released app nor touch what it has saved.
# DBDELVE_CHANNEL=release builds the app the release workflow ships.
#
# Usage: dev/bundle.sh
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

VERSION="$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)"
CHANNEL="${DBDELVE_CHANNEL:-dev}"

if [[ "$CHANNEL" == release ]]; then
  APP="target/DBDelve.app"
  # CFBundleIdentifier is what the Keychain scopes saved profile passwords to.
  # Changing it orphans every password already stored.
  IDENTIFIER="com.shayanabbas.dbdelve"
  # Capitalised: the menu bar, Force Quit and Activity Monitor all name the app
  # after its executable, not after CFBundleName.
  NAME="DBDelve"
  ENVIRONMENT=""
else
  APP="target/macos-dev/DBDelve Dev.app"
  IDENTIFIER="com.shayanabbas.dbdelve.dev"
  NAME="DBDelve Dev"
  # Baked into the plist rather than exported here: the isolation has to hold
  # for a launch from the Dock, from Finder, or from a crash reporter's
  # "Quit & Reopen", none of which inherit the environment of this shell.
  # Leading newline here rather than in the heredoc: a heredoc expands
  # parameters but not $'...', so the line break has to arrive already made.
  ENVIRONMENT=$'\n\t<key>LSEnvironment</key>\n\t<dict><key>DBDELVE_VARIANT</key><string>dev</string></dict>'

  # Before the build, not after it: writing over the executable of a running
  # process is what breaks it. Only the dev instance is guarded -- the released
  # DBDelve.app is meant to stay open alongside this one.
  if pgrep -f "$ROOT/$APP/Contents/MacOS/" >/dev/null; then
    echo "a dev instance is already running -- quit it and re-run" >&2
    exit 1
  fi
fi

cargo build --release
# Builds the ODBC bridge, which links unixODBC statically on macOS, and stages
# it under target/release/plugins/odbc.
dev/plugins.sh --release

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources/plugins/odbc" "$APP/Contents/Helpers"

cp assets/macos/DBDelve.icns "$APP/Contents/Resources/DBDelve.icns"
cp target/release/dbdelve "$APP/Contents/MacOS/$NAME"

# The OFL asks that the licence travel with the fonts, and the fonts are
# compiled into the binary above -- so the notices ship inside the bundle
# rather than only sitting in the repository.
cp NOTICES.md "$APP/Contents/Resources/"
cp -R licenses "$APP/Contents/Resources/"

# The manifest goes where db::plugin::discover() reads manifests in a bundle
# (Resources/plugins/<id>/), and the executable goes in Helpers/, where
# codesign expects nested code: one under Resources is treated as an unsealed
# resource rather than code to be signed.
cp target/release/plugins/odbc/dbdelve-plugin.toml "$APP/Contents/Resources/plugins/odbc/"
BRIDGE="$APP/Contents/Helpers/dbdelve-odbc"
cp target/release/plugins/odbc/bin/dbdelve-odbc "$BRIDGE"

# unixODBC is compiled into the bridge: one linked to a Homebrew libodbc runs
# here and fails on every machine without it.
if otool -L "$BRIDGE" | tail -n +2 | grep -E 'odbc|/opt/homebrew|/usr/local'; then
  echo "the bridge links a library outside the system: unixODBC must be static" >&2
  exit 1
fi

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleName</key><string>${NAME}</string>
	<key>CFBundleDisplayName</key><string>${NAME}</string>
	<key>CFBundleIdentifier</key><string>${IDENTIFIER}</string>
	<key>CFBundleExecutable</key><string>${NAME}</string>
	<key>CFBundleIconFile</key><string>DBDelve</string>
	<key>CFBundlePackageType</key><string>APPL</string>
	<key>CFBundleShortVersionString</key><string>${VERSION}</string>
	<key>CFBundleVersion</key><string>${VERSION}</string>
	<key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
	<key>LSMinimumSystemVersion</key><string>12.0</string>
	<key>LSApplicationCategoryType</key><string>public.app-category.developer-tools</string>
	<key>NSHighResolutionCapable</key><true/>${ENVIRONMENT}
</dict>
</plist>
PLIST
plutil -lint -s "$APP/Contents/Info.plist"

# An arm64 binary will not launch without a signature, and copying it into the
# bundle invalidates the one rustc left behind. DBDELVE_SIGN_ID takes a real
# identity when there is one to distribute under, and dev/identity.sh's
# self-signed one is what keeps the Keychain from re-asking after every
# rebuild. Ad-hoc is the fallback, and it runs -- it just prompts.
DEV_IDENTITY="DBDelve Dev Signing"
if [[ -z "${DBDELVE_SIGN_ID:-}" ]] &&
  security find-certificate -c "$DEV_IDENTITY" >/dev/null 2>&1; then
  DBDELVE_SIGN_ID="$DEV_IDENTITY"
fi
# --identifier spelled out rather than inferred from CFBundleIdentifier: the
# Keychain pins its "Always Allow" to the signature's identifier, so the one
# thing that must not drift between builds is the one thing stated here.
#
# The bridge is signed first, under its own identifier, because signing the
# bundle seals what is inside it as it stands.
#
# Once the app is notarized (hardened runtime), the bridge, and only the
# bridge, will need the com.apple.security.cs.disable-library-validation
# entitlement: it loads vendor ODBC drivers signed by other teams, and the app
# must keep library validation. Not added yet: nothing here enables the
# hardened runtime.
codesign --force --sign "${DBDELVE_SIGN_ID:--}" --identifier "$IDENTIFIER.odbc" "$BRIDGE"
codesign --force --sign "${DBDELVE_SIGN_ID:--}" --identifier "$IDENTIFIER" "$APP"
codesign --verify --deep --strict "$APP"

# Neither variant installs. The release reaches /Applications by the DMG's
# drag-to-install, and the dev app runs where it was built so it can never be
# mistaken for -- or dropped over -- the real one.
if [[ "$CHANNEL" == release ]]; then
  echo "built $APP (v$VERSION)"
  exit 0
fi

# open rather than exec: LaunchServices owns the process, so the app outlives
# this script and behaves like the Dock launched it.
open "$APP"
echo "launched $APP (v$VERSION)"
