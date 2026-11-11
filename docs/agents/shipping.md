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

## Self-update (macOS, Linux and Windows)

`src/update.rs` checks GitHub's `releases/latest` at launch and every six
hours. An install that can replace itself gets **Update and restart**; every
other one gets the link to the release page. What the platforms share --
the release key, the download, the minisign check, the install lock and the
scratch directory -- is in `src/update/download.rs`; the file identity Linux
and Windows compare is in `src/update/identity.rs`. All fetch the
`.minisig` before the asset, so a release without one fails before the large
download, and verify once the download is complete. The pill and its menu sit
at the right end of the titlebar on the home screen as well as in a
connection, and the palette lists the update on both.

On macOS (`src/update/macos.rs`) the update downloads
`DBDelve-<version>.dmg`, verifies `DBDelve-<version>.dmg.minisig` against
`RELEASE_PUBLIC_KEY`, copies the app out of the image to a hidden sibling,
checks it (`codesign --verify --deep --strict`, same `CFBundleIdentifier`,
`CFBundleShortVersionString` equal to the release and newer than both the
running build and the installed bundle), exchanges the two in one
`renamex_np(RENAME_SWAP)` and relaunches through `App::restart`, which quits
the way Quit does. A volume without the atomic exchange fails the update and
keeps the release link: moving the app aside first would leave nothing
installed if the process died in between.

On Linux (`src/update/linux.rs`) two kinds of install update themselves:

- **An AppImage** downloads `dbdelve-<version>-<arch>.AppImage`, copies it to
  a hidden sibling of the AppImage (same filesystem), checks it is an ELF and
  `rename(2)`s it over the old one under the old one's name. The running
  image keeps its inode until it exits. It counts as an AppImage only when
  `APPIMAGE` names a regular file *and* the executable lies inside `APPDIR`:
  the runtime exports both to everything an AppImage starts, so a terminal
  opened from one hands them to an unrelated DBDelve. The relaunch inherits
  the old mount's `LD_LIBRARY_PATH`; AppRun restores the original from the
  `DBDELVE_ORIGINAL_LD_LIBRARY_PATH` pair the earlier AppRun saved before
  saving it again, so the new process sees the same as a fresh start.
  Before the relaunch every descriptor past stdio is made close-on-exec: the
  runtime keeps its FUSE mount alive while any process holds a pipe it hands
  the app, and the relaunch would otherwise keep the old mount, and the
  replaced file under it, alive until the new app exits. An AppImage run with
  `--appimage-extract-and-run` (its `APPDIR` is not a mount point) keeps the
  link, since the plain relaunch would need the FUSE it ran without; with
  `APPIMAGE_EXTRACT_AND_RUN` set it updates, as the relaunch inherits that.
- **The tarball's `install.sh` install**, recognised only when the executable
  is exactly `~/.local/bin/dbdelve` (a regular file, not a link), downloads
  `dbdelve-<version>-linux-<arch>.tar.gz`, unpacks it with the `tar` crate
  (no entry may land outside the scratch directory), requires the one
  directory `dbdelve-<version>-linux-<arch>` holding `dbdelve` and
  `install.sh`, and runs that `install.sh`: the new release knows its own
  layout. `install.sh` copies the binary beside where it goes and the ODBC
  bridge beside `plugins/` (never in it, where discovery would load it), and
  renames both into place only once both copies are whole, so a failed copy
  changes nothing and a running copy keeps its file. The commit is renames
  only, with HUP, INT and TERM ignored while it runs; a failed rename puts
  the old bridge back, and a run killed hard part way through leaves a backup
  the next run restores before doing anything else. A lock file of its own,
  `.install.lock` in the data directory (taken when `flock` exists; never the
  `~/.local/bin` lock the updater holds), keeps two runs from sharing the
  staged and backup names. Plugin discovery skips hidden folders, so neither
  copy is ever loaded.

Everything else on Linux -- a tarball run where it was unpacked, a distro or
Snap package, a binary anywhere else -- keeps the link.

On Windows (`src/update/windows.rs`) a running executable can be renamed but
not overwritten or deleted, so every update moves what it replaces into a
folder ending `.dbdelve-old` inside the install, and that folder is cleared
later. The download goes to `%LOCALAPPDATA%\dbdelve\updates`, not the
temporary folder, which can be pointed somewhere shared, and stays open from
before its signature is checked until Setup has exited or the zip is
unpacked, shared for reading only, so nothing can swap the verified file for
another. Two kinds of install update themselves:

- **The installer's install**, recognised when the running `dbdelve.exe`'s
  folder is the one Inno Setup recorded under
  `Software\Microsoft\Windows\CurrentVersion\Uninstall\{300CEAF2-D7A2-4D97-B94B-20ED16631AF4}_is1`
  (`Inno Setup: App Path`, else `InstallLocation`), looked up in HKCU, then
  HKLM's 64-bit view. The hive that matched is the mode. The update
  downloads `dbdelve-<version>-windows-<arch>-setup.exe` and runs it,
  waiting for its exit code, as
  `/VERYSILENT /SUPPRESSMSGBOXES /NORESTART /NOCLOSEAPPLICATIONS /CURRENTUSER|/ALLUSERS /UPDATE "/DIR=<folder>" "/LOG=<scratch>\setup.log"`.
  `/ALLUSERS` brings the UAC prompt; Setup's unelevated half waits for its
  elevated self and passes its exit code on, and a declined prompt exits 2,
  which the app reports as cancelled and lets the user retry. Any other
  non-zero code is a failure with the log's tail in the app's log. After
  exit 0 the app checks a different file now stands at its path, then
  restarts into it through `App::restart`; the app itself is never elevated,
  so neither is the relaunch, and the script has no `[Run]` relaunch.
- **The portable zip**, recognised when it is not the installer's, the
  executable is `dbdelve.exe`, `LICENSE` and `NOTICES.md` sit beside it (a
  build tree has neither) and the folder is writable (probed by creating a
  file: Windows has no `access(2)`). It downloads
  `dbdelve-<version>-windows-<arch>.zip`, unpacks it into
  `update.dbdelve-new` inside the install (same volume) with the `zip` crate,
  and requires every entry to sit under the one folder
  `dbdelve-<version>-windows-<arch>` through plain names (no root, drive,
  `..`, `:` or link) and a `dbdelve.exe` in it. Then it swaps file by file:
  each installed file the release ships is renamed into the set-aside
  folder, `plugins\odbc` is emptied the same way and removed (a folder
  holding a running bridge cannot itself be renamed), and the release's
  files and its `plugins\odbc` folder are renamed in. Any failed step undoes
  every rename before it, in reverse. Files the release does not ship stay,
  so the user's own files and an older release's dropped ones remain; the
  folder keeps its old versioned name.

`/UPDATE` is the installer contract, in `assets/windows/dbdelve.iss`'s
`[Code]`. With it, `PrepareToInstall` (which runs before Setup looks for
files in use and before `[InstallDelete]`) renames `dbdelve.exe`,
`plugins\odbc\bin\dbdelve-odbc.exe` and then the whole `plugins` folder into
`{app}\<timestamp>.dbdelve-old`; the elevated installer does it, since the
app cannot write Program Files. `[InstallDelete]` then finds no `plugins` to
wipe (if `plugins` would not move, it wipes it as before). If Setup ends
without reaching `ssPostInstall`, `DeinitializeSetup` clears whatever the
rolled-back install left at each path and renames everything back, in
reverse. Without `/UPDATE` nothing is moved, so an interactive run is as it
was. Every run clears earlier `*.dbdelve-old` entries in `PrepareToInstall`
and after installing, and `[UninstallDelete]` removes them on uninstall. The
app clears them, and a leftover `update.dbdelve-new`, at startup where it can
write (not Program Files), under the install lock and best effort: the
executable an update has just replaced is still running until the relaunch.
The script ships in the release it installs, so it is the new release's
`[Code]` that runs.

Every platform takes a lock for the folder of what it replaces, which keeps
two processes from clearing each other's hidden sibling or set-aside files,
and clears the one an interrupted install left. On macOS and Linux it is
`flock` on that folder; on Windows it is a file opened with no sharing, in the
user's temporary folder, named after a hash of the install folder (an
all-users install's folder is not writable without elevation). Being per
user, it does not keep two users from updating one all-users install at
once; the elevated installer is all that serialises them. Under that lock
all refuse an install that another process has updated since this one
started (Windows checks again once the update has downloaded and verified,
just before it changes anything), so a stale offer never puts an older
release over a newer one: macOS by the installed bundle's version, Linux and Windows by the file itself (the
AppImage as recorded at startup, before any update check, or
`/proc/self/exe` for `~/.local/bin/dbdelve`, by device and inode; on Windows
`dbdelve.exe` as recorded at startup, by volume serial number and file index).
On Linux and Windows a replaced install also stops offering the update, and
when the refusal comes at install time the menu offers **Restart DBDelve**,
which relaunches the install on disk, rather than a retry that could never
succeed.

- **Only the release build in a writable place updates itself.** A dev or
  other `DBDELVE_VARIANT` build, a binary outside a `.app`, a bundle on a
  read-only volume (run from the DMG), App-Translocated, or in a folder the
  user cannot write, keeps the link to the release page; so does an AppImage
  or a portable Windows folder the user cannot write. So does a release
  missing the asset or its `.minisig`, and a build whose key is still the
  placeholder.
- **What is tested where.** The Windows module's logic (kind detection with
  the registry injected, the installer's command line, zip validation, the
  swap with an open file and its rollback at each stage, set-aside clearing,
  the replaced-file check) runs on macOS too, as `linux.rs`'s does; the
  identity, lock and writability calls run only on the Windows CI job.
- **The Windows update end to end.** `update-e2e-windows.yml` runs
  `dev/update-e2e-windows.ps1` on pushes that touch the updater, the
  installer script or the packaging, and from the Actions tab. It builds this
  version and 99.0.0 with `update-test-hooks`, signs the newer with a key made
  for the run, serves a `releases/latest`-shaped feed on 127.0.0.1, and lets
  the older app update itself (`DBDELVE_UPDATE_AUTO=1`, which only a test
  build reads) from a per-user install with the ODBC bridge running, an
  all-users install, and a portable folder holding files of the user's own;
  each must end with the new files in place, the new app running from the
  same path, user files kept and nothing set aside left behind. A badly
  signed update must leave the install untouched, and an installer forced to
  fail after `/UPDATE` set the running files aside must put them back and
  leave the old app runnable. The runner is already elevated, so the UAC
  prompt, declining it, and DBDelve reopening unelevated afterwards are still
  checked by hand. By hand, on a Windows machine without DBDelve installed:
  `pwsh -File dev/update-e2e-windows.ps1` (elevated for the all-users case;
  `-SkipBuild` reuses the last builds, `-Only portable` runs one scenario).
- **A signature binds the asset's name.** minisign's default trusted comment
  is `timestamp:<n>\tfile:<name>\thashed`, and the signature covers it. The
  app refuses a signature whose `file:` is not the exact asset it asked for,
  which carries the version, so an older release's genuine file and signature
  served under the new name do not verify. The release must keep signing
  with the default comment (no `-t`); the workflow checks it does.
- **The release workflow signs every asset** with the `UPDATE_SIGNING_KEY`
  secret (an unencrypted minisign secret key) and checks each signature is
  prehashed, names its asset and verifies against the key in
  `src/update/download.rs`; it refuses to publish otherwise. Changing the key
  strands every installed app on the old one, which then never verifies
  another release.
- **The cask says `auto_updates true`**, so `brew upgrade` does not fight the
  app over which copy is current.
- **`update-test-hooks` is the only way to point an app elsewhere.** Off by
  default; with it the app reads `DBDELVE_UPDATE_FEED`,
  `DBDELVE_UPDATE_PUBLIC_KEY` and `DBDELVE_UPDATE_AUTO` (install a found
  update without the click), and a variant build may update itself.
  `DBDELVE_FEATURES=update-test-hooks DBDELVE_NO_LAUNCH=1 dev/bundle.sh`
  builds such a dev bundle without opening it.

## The ODBC bridge in each artifact

Every artifact carries `plugins/odbc` (never echo, which is test code), laid
out where `db::plugin::discover()` looks. `dev/plugins.sh --release` builds it
(`cargo build --release -p dbdelve-plugin-odbc`) and stages it; `bundle.sh`,
`package-linux.sh` and `package-appimage.sh` copy from that stage, while
`package-windows.ps1` builds the bridge itself and copies from `target`. The driver manager differs per platform:

| artifact | where | driver manager |
| --- | --- | --- |
| macOS `.app` | manifest in `Contents/Resources/plugins/odbc/`, executable in `Contents/Helpers/dbdelve-odbc` | unixODBC, compiled into the bridge |
| Linux tarball | `plugins/odbc/` beside `dbdelve`; `install.sh` copies it to `~/.local/share/dbdelve/plugins/odbc` (the binary lands alone in `~/.local/bin`) | system `libodbc.so.2` |
| AppImage | `usr/bin/plugins/odbc/` | system `libodbc.so.2` |
| Windows zip | `plugins/odbc/` beside `dbdelve.exe` | system `odbc32` |
| Windows installer | `plugins/odbc/` beside `dbdelve.exe` | system `odbc32` |

- **macOS signing order.** The helper is signed first, with its own
  identifier (`$IDENTIFIER.odbc`), then the bundle, and the bundle is verified
  with `--deep --strict`. `bundle.sh` runs `otool -L` on the helper and fails
  the bundle if it links anything named `odbc` or under `/opt/homebrew` or
  `/usr/local`: a Homebrew unixODBC would run on the build machine and nowhere
  else. Once the app is notarized, the bridge, and only the bridge, needs
  `com.apple.security.cs.disable-library-validation`, because it loads vendor
  drivers signed by other teams. It is not added yet, since nothing enables the
  hardened runtime.
- **unixODBC is LGPL-2.1-or-later**, redistributed only in the macOS bundle;
  `NOTICES.md` and `licenses/LGPL-2.1.txt` ship with it.
- **Linux needs the `unixodbc` package** (`apt install unixodbc`) to connect
  through ODBC. `libodbc.so.2` is never bundled, not even in the AppImage:
  without it only the bridge fails to start, and the rest of the app is
  unaffected. The build runner needs `unixodbc-dev`.
- **Windows ships the bridge untested**: it is built in CI and packed into
  both the zip and the installer, never run. The release notes say so.

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
  release runs on. libstdc++ and libgcc_s come from the host too, and
  `dev/check-linux-floor.sh` fails the build if either binary needs symbol
  versions newer than 22.04 ships;
- the macOS DMG: `dev/bundle.sh` with `DBDELVE_CHANNEL=release DBDELVE_SIGN_ID=-`,
  plus an `/Applications` symlink;
- the Windows zip and installer (`dev/package-windows.ps1`), unsigned. The
  installer is Inno Setup: per-user it writes `%LOCALAPPDATA%\Programs\DBDelve`,
  all-users `Program Files\DBDelve`. AppId
  `{300CEAF2-D7A2-4D97-B94B-20ED16631AF4}` is how upgrades find the previous
  copy; changing it stacks a second install. SmartScreen warns because nothing
  is signed, the same trade as the macOS ad-hoc signature.

Only once every asset exists does it tag the commit `v$VERSION` and publish,
then it rewrites `Casks/dbdelve.rb` in the `ShayanAbbas1/homebrew-dbdelve` tap
through the `TAP_TOKEN` secret. A version with a hyphen (`0.2.0-rc.1`) is a
pre-release, may run from any branch and leaves the cask alone. Unticked, the
workflow builds everything, signs it and publishes nothing, which is how to
check `UPDATE_SIGNING_KEY` against the app's key without releasing.

**There is no notarization and no Developer ID.** A release signs ad-hoc rather
than with `dev/identity.sh`'s certificate: that certificate is trusted only on
the machine that created it, and to Gatekeeper an issuer nobody trusts reads
worse than no issuer at all. The trade is that the ad-hoc hash moves with every
release, so an update costs one fresh Keychain prompt. The cask clears quarantine
itself in a `postflight_steps` block; a `.dmg` installed by hand still needs
`xattr -dr com.apple.quarantine /Applications/DBDelve.app`.

