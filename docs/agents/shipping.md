# Bundling and releasing

## Bundling (macOS)

`dev/bundle.sh` builds `--release`, copies in the icon, writes `Info.plist`
and signs. It is the only way to get a real app rather than a binary. It
builds **two variants from one script**, and neither of them installs:

- **Dev, the default.** `target/macos-dev/DBDelve Dev.app`, named
  `DBDelve Dev`, id `com.shayanabbas.dbdelve.dev`. Run from the build tree,
  never copied to `/Applications`, and launched with `open` so LaunchServices
  owns it. The script refuses to build while a dev instance is running
  (overwriting a running executable is what breaks it) but says nothing about
  the released app, which is meant to stay open alongside.
- **Release, under `DBDELVE_CHANNEL=release`.** `target/DBDelve.app`, named
  `DBDelve`, id `com.shayanabbas.dbdelve`, no `LSEnvironment`. Only the release
  workflow sets it, and the DMG's drag-to-Applications is the install.

**The two variants share nothing on disk.** `DBDELVE_VARIANT` moves both the
support directory and the keychain service together: unset or empty is
`dbdelve` (the release's, never to move), `dev` is `dbdelve-dev`. Both, always:
a build that suffixed only one would read the release's saved passwords or
write its `profiles.toml`. `store::variant_name` is the single place it is
decided. It validates the variant with the same `unsafe_component` every path
component goes through, and a variant it rejects is an error rather than a fall
back to the release name, because silently falling back is exactly how a dev
build corrupts the real one.

The dev variant carries `DBDELVE_VARIANT=dev` in an `LSEnvironment` dict in
its own `Info.plist` rather than exported by the script, because the isolation
has to survive a launch from the Dock, from Finder, or from a crash reporter's
"Quit & Reopen", none of which see the shell that built the app.

Four things in the script are load-bearing:

- **`CFBundleIdentifier` scopes the Keychain.** Every saved profile password
  belongs to `com.shayanabbas.dbdelve`. Changing it orphans all of them. The
  dev id differs precisely so the two cannot reach each other's prompts.
- **The signature is not optional on arm64.** An unsigned arm64 binary will not
  launch, and copying the binary into the bundle invalidates the signature
  rustc left. `DBDELVE_SIGN_ID` takes a real identity; `dev/identity.sh`'s
  self-signed one (used automatically once it exists) is what stops the
  Keychain re-prompting after every rebuild; ad-hoc is the fallback and runs,
  but prompts.
- **`codesign --identifier` is passed explicitly.** The Keychain pins its
  "Always Allow" to the signature's identifier, so the one value that must not
  drift between builds is stated rather than inferred.
- **The font licences ship inside the bundle**, because the fonts are compiled
  into the binary and the OFL asks the licence to travel with them.

## Linux desktop identity

Packagers can set `DBDELVE_APP_ID` at launch to match their installed desktop
entry's basename without `.desktop`, so Wayland associates the window with
the entry and its icon. For `dbdelve_dbdelve.desktop`, set
`DBDELVE_APP_ID=dbdelve_dbdelve`. Unset or empty keeps the default `dbdelve`.

## Releasing

A release is cut from the Actions tab: run `.github/workflows/release.yml` with
**publish** ticked. The version comes from `Cargo.toml`, so bump it first: a
version that is already tagged fails before anything builds, and a stable
version must run from main. It builds, in parallel:

- Linux x86_64 and aarch64 tarballs and AppImages (`dev/package-linux.sh`,
  `dev/package-appimage.sh`) on Ubuntu 22.04, never `ubuntu-latest`: glibc is
  forward-compatible only, so the oldest runner is the oldest system the
  release runs on;
- the macOS DMG: `dev/bundle.sh` with `DBDELVE_CHANNEL=release DBDELVE_SIGN_ID=-`,
  plus an `/Applications` symlink;
- the Windows zip (`dev/package-windows.ps1`), unsigned.

Only once every asset exists does it tag the commit `v$VERSION` and publish,
then it rewrites `Casks/dbdelve.rb` in the `ShayanAbbas1/homebrew-dbdelve` tap
through the `TAP_TOKEN` secret. A version with a hyphen (`0.2.0-rc.1`) is a
pre-release, may run from any branch and leaves the cask alone. Unticked, the
workflow builds everything and publishes nothing.

**There is no notarization and no Developer ID.** A release signs ad-hoc rather
than with `dev/identity.sh`'s certificate: that certificate is trusted only on
the machine that created it, and to Gatekeeper an issuer nobody trusts reads
worse than no issuer at all. The trade is that the ad-hoc hash moves with every
release, so an update costs one fresh Keychain prompt. Installing still means
clearing quarantine by hand:
`xattr -dr com.apple.quarantine /Applications/DBDelve.app`.

