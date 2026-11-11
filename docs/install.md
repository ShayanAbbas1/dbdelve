# Installing DBDelve

## macOS

Apple Silicon, macOS 12 or later — that's what it's built and tested on. Intel
and older macOS aren't blocked by anything in the code, they're just untested.

Homebrew is the recommended way to install it:

```sh
brew install --cask ShayanAbbas1/dbdelve/dbdelve
```

DBDelve is signed ad-hoc: there's no Developer ID behind it and nothing is
notarized, so Gatekeeper declines to open it while the download's quarantine
flag is set. The cask clears that flag as it installs.

After that it updates itself: when a release is out, the titlebar says so, and
**Update and restart** in its menu downloads it, checks its signature, replaces
the app and opens the new one. That needs the app to live somewhere you can
write to, such as `/Applications` on an administrator account; one run from the
`.dmg`, or from a folder you can't write to, links to the release instead.

Your connections and query history are kept outside the app and survive updates.
macOS will ask once more for the saved passwords in your Keychain after each
update, because each release carries a different ad-hoc signature. I'll
pay for a Developer ID if enough people end up using this, which removes all of
the above.

Without Homebrew, one line installs the latest release into `/Applications`
(or `~/Applications` when that isn't yours to write to):

```sh
curl -fsSL https://raw.githubusercontent.com/ShayanAbbas1/dbdelve/main/install.sh | sh
```

Nothing it downloads is quarantined, so the app just opens. If `minisign` is
installed it also checks the release's signature, and installs nothing that
fails the check.

Or take the `.dmg` from [the latest release][releases] and drag DBDelve to
Applications. A browser quarantines what it downloads and nothing clears that
for you this way, so do it once before opening the app:

```sh
xattr -dr com.apple.quarantine /Applications/DBDelve.app
```

From then on it updates itself the same way.

To build it yourself instead, `DBDELVE_CHANNEL=release dev/bundle.sh` produces
`target/DBDelve.app` with the release build, icon and signature. Drag that to
Applications; the quarantine step doesn't apply here.

## Linux

One line installs the latest release for your architecture under `~/.local`,
with no root:

```sh
curl -fsSL https://raw.githubusercontent.com/ShayanAbbas1/dbdelve/main/install.sh | sh
```

It downloads the tarball described below and runs the `install.sh` inside it.
If `minisign` is installed it also checks the release's signature, and installs
nothing that fails the check. It needs `curl`, `tar` and `bash`. An install made
this way updates itself, see [Updating](#updating).

Or install from the AUR:

```sh
yay -S dbdelve-bin
```

It's maintained by
[@0PandaDEV](https://github.com/0PandaDEV), not by this project. Thanks for
packaging it!

A tarball or an AppImage, x86_64 or aarch64, built on Ubuntu 22.04 — that is
the oldest glibc either will run against, so anything at least that new is
fine. X11 and Wayland both work, and you need a Vulkan or OpenGL driver, a
Secret Service keyring (gnome-keyring or KWallet) for saved passwords, and
xdg-desktop-portal for the export dialog.

Take `dbdelve-<version>-linux-<arch>.tar.gz` from [the latest release][releases]
and run the `install.sh` inside it:

```sh
tar xzf dbdelve-*-linux-*.tar.gz
cd dbdelve-*-linux-*/
./install.sh
```

Everything goes under `~/.local`, so no root: the binary to `~/.local/bin`, the
icons and the desktop entry where your desktop looks for them, which is what
puts DBDelve in the app menu. Make sure `~/.local/bin` is on your PATH —
`install.sh` says so if it isn't. The installed app updates itself, and
running the next release's `install.sh` over it works too;
`./install.sh --uninstall` reverses it and leaves your connections and query
history alone.

Or take `dbdelve-<version>-<arch>.AppImage` from the same release, which is one
file that runs where it lands:

```sh
chmod +x dbdelve-*.AppImage
./dbdelve-*.AppImage
```

It carries the libraries that are safe to carry and takes the graphics drivers
from your machine, so it runs on distributions either side of the one it was
built on. Running any AppImage needs FUSE 2, which Ubuntu 22.04 and later no
longer install — `sudo apt install libfuse2`, or `libfuse2t64` on 24.04 and
later, is usually the only thing missing. Failing that,
`./dbdelve-*.AppImage --appimage-extract` unpacks it and `squashfs-root/AppRun`
runs it with no FUSE at all.

What it does not do is put DBDelve in the app menu: an AppImage on its own is
just a file you run, so there is no desktop entry and no icon until you
integrate it — Gear Lever and AppImageLauncher both do that, or you copy the
entry out by hand. The tarball does it for you, which is still the reason to
prefer it. An AppImage in a folder you can write to updates itself in place,
see [Updating](#updating).

To build it yourself instead, on Debian or Ubuntu the build needs:

```sh
sudo apt install build-essential pkg-config cmake libfontconfig-dev \
  libxkbcommon-dev libxkbcommon-x11-dev libwayland-dev libdbus-1-dev
cargo build --release
```

The binary lands at `target/release/dbdelve` and runs from anywhere;
`dev/package-linux.sh` wraps that same build into the tarball above and
`dev/package-appimage.sh` into the AppImage. Connections and query history go
to `$XDG_DATA_HOME/dbdelve`, or `~/.local/share/dbdelve`.

## Windows

x86_64, from [the latest release][releases]. The installer is the
recommended way. It asks whether to put DBDelve in AppData (just for you, no
administrator) or Program Files (all users). Connections and query history
stay in `%APPDATA%\dbdelve` either way, and uninstall leaves them there.

```text
dbdelve-<version>-windows-x86_64-setup.exe
```

The installer is unsigned, so SmartScreen will warn on the first run: More
info, then Run anyway. A Start Menu shortcut is created; a desktop one is
offered and off by default.

Failing that, the zip is the portable build: unzip it and run `dbdelve.exe`.
Nothing is installed.

To build it yourself instead, the build needs the MSVC toolchain and the
Windows SDK, which is what compiles the bundled SQLite and GPUI's shaders:

```sh
cargo build --release
```

The binary is `target\release\dbdelve.exe`. `dev/package-windows.ps1` wraps
that same build into the zip above, and into the installer if [Inno Setup][]
is on the machine (`choco install innosetup`). Passwords go in Credential
Manager. The window draws its own title bar, buttons included. Chords are
the same as macOS with Ctrl in place of Cmd.

## Other databases through ODBC

Databases without a built-in engine, such as IBM Db2, connect through the ODBC
bridge that ships with DBDelve. You install your database's own ODBC driver;
DBDelve doesn't bundle any.

- **Linux** needs the unixODBC package, which provides `libodbc.so.2`:
  `sudo apt install unixodbc`. The tarball and the AppImage both use it.
- **macOS** carries its own unixODBC, which reads `/etc/odbcinst.ini` and not
  Homebrew's configuration. Put the driver's absolute path in the connection
  string, `DRIVER=/absolute/path/to/lib.dylib`, instead of a driver name. An app
  opened from Finder doesn't get your shell's environment variables either, so
  `ODBCSYSINI` and the like have no effect.
- A driver downloaded with a browser may be quarantined, and macOS then refuses
  to load it: `xattr -dr com.apple.quarantine <driver directory>`.
- **Windows** uses the system ODBC manager. That path is untested.

## Updating

On macOS and Linux, **Update and restart** in the titlebar's update menu (or
"Update to dbdelve … and restart" in the command palette) does it in one click.
It downloads the release from GitHub, checks its signature against the key built
into the app, replaces the app and opens the new one. On macOS that is
described [above](#macos). Homebrew leaves a self-updating app to update
itself, so `brew upgrade` skips DBDelve unless you add `--greedy`.

On Linux it works for an install at `~/.local/bin/dbdelve`, which is what the
tarball's `install.sh` and the one-liner make, and for a running AppImage whose
folder you can write to. Installs from the AUR or another package manager, a
tarball run from where it was unpacked, and builds from source don't update
themselves; the titlebar links to [the latest release][releases] instead, and
your package manager is the way to update.

Windows doesn't update itself yet. If you used the Windows installer, run
the next one over it; it upgrades in place. The zip has no upgrade: each
version unzips to its own folder, so delete the old one. Connections and any
plugins you added live in `%APPDATA%\dbdelve`, not there. The titlebar links
to [the latest release][releases] once there is a newer one.

There is no telemetry. The only requests DBDelve makes on its own are to
GitHub, to check for a newer release at startup and every six hours after,
which you can turn off in Settings, and to download the release you asked to
update to, on macOS and Linux.

[releases]: https://github.com/ShayanAbbas1/dbdelve/releases/latest
[Inno Setup]: https://jrsoftware.org/isinfo.php
