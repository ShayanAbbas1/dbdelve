#!/usr/bin/env bash
#
# Builds DBDelve for Linux and wraps it in a single-file AppImage: the same
# release binary, icons and desktop entry dev/package-linux.sh ships, laid out
# the way appimagetool expects and carrying the handful of libraries that are
# safe to travel.
#
# Bare appimagetool rather than linuxdeploy: linuxdeploy's whole value is
# working the dependency closure out for you, and that is precisely the part we
# do not want done for us. It copies everything ldd reports minus a blocklist,
# so getting to the split below means fighting it with exclude flags and
# hoping the blocklist it ships agrees with us next release. The set of
# libraries DBDelve may carry is short enough to state outright, and stating it
# outright is the only way a reader can check it.
#
# Usage: dev/package-appimage.sh
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

VERSION="$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)"
# Same reason as dev/package-linux.sh: a build that set CARGO_TARGET_DIR put
# the binary somewhere else, and reading `target/` regardless picks up a stale
# one.
TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"
ARCH="$(uname -m)"
APPDIR="$TARGET_DIR/linux/dbdelve-$VERSION-$ARCH.AppDir"
APPIMAGE="$TARGET_DIR/linux/dbdelve-$VERSION-$ARCH.AppImage"

cargo build --release
# The bridge links the system's libodbc.so.2. It is deliberately not bundled:
# the loop below only reads the app binary's dependencies, and a user needs the
# unixodbc package (apt install unixodbc) to connect through ODBC. Without it
# only the bridge fails to start.
dev/plugins.sh --release
dev/check-linux-floor.sh "$TARGET_DIR/release/dbdelve" "$TARGET_DIR"/release/plugins/odbc/bin/*

rm -rf "$APPDIR"
mkdir -p "$APPDIR/usr/bin" "$APPDIR/usr/lib" "$APPDIR/usr/share/applications"
install -m 755 "$TARGET_DIR/release/dbdelve" "$APPDIR/usr/bin/dbdelve"
# Beside the binary, where db::plugin::discover() looks; only odbc, as echo is
# test code that may sit in the staging directory.
mkdir -p "$APPDIR/usr/bin/plugins"
cp -R "$TARGET_DIR/release/plugins/odbc" "$APPDIR/usr/bin/plugins/odbc"

# The basename has to stay `dbdelve`: src/main.rs asks gpui for the app_id
# "dbdelve", that app_id is what a Wayland compositor matches against a desktop
# entry's basename, and a rename here is a window with no icon.
install -m 644 assets/linux/dbdelve.desktop "$APPDIR/usr/share/applications/dbdelve.desktop"
for PNG in assets/linux/icons/dbdelve-*.png; do
  SIZE="${PNG##*-}"
  SIZE="${SIZE%.png}"
  install -Dm 644 "$PNG" "$APPDIR/usr/share/icons/hicolor/${SIZE}x${SIZE}/apps/dbdelve.png"
done
# appimagetool reads the desktop entry and the icon from the AppDir root, and
# the thumbnailers read .DirIcon. All three are the files above under another
# name.
cp assets/linux/icons/dbdelve-256.png "$APPDIR/dbdelve.png"
cp "$APPDIR/dbdelve.png" "$APPDIR/.DirIcon"
ln -s usr/share/applications/dbdelve.desktop "$APPDIR/dbdelve.desktop"

cp LICENSE NOTICES.md "$APPDIR/"
# NOTICES.md points at licenses/OFL-1.1.txt, and the OFL asks that the licence
# travel with the fonts -- which are compiled into the binary above.
cp -R licenses "$APPDIR/"

# Which libraries ride along, and which must come from the host. Get this
# wrong and you get an AppImage that runs perfectly on the machine that built
# it and dies on every other one, so the rule is stated rather than inferred:
# everything ldd resolves is bundled except the families below.
#
#   glibc (libc, libm, libdl, libpthread, librt, libresolv, ld-linux)
#     Bundling glibc means the bundled loader and the host's NSS modules
#     disagree, and a mismatch there breaks DNS, users and locales rather than
#     failing to start. The build runs on Ubuntu 22.04 for exactly this reason:
#     glibc is forward-compatible, so the oldest build host is the floor.
#
#   libstdc++ and libgcc_s
#     Backward-compatible the same way glibc is, and needed by the host's GPU
#     stack as much as by us: Mesa's Vulkan drivers link SPIRV-Tools and LLVM,
#     both C++, built against the host's libstdc++. A process holds one copy
#     of it, so a bundled older one is the copy the driver gets, and on any
#     distribution newer than the build host the driver then fails to load
#     and the window never opens (#213). dev/check-linux-floor.sh keeps the
#     binary from needing a newer one than 22.04 ships.
#
#   GL, EGL, GLX, GLdispatch, GLES, glapi, gbm, libdrm, Vulkan loader and ICDs
#     These have to be the ones that match the running kernel's DRM driver.
#     A bundled libGL loads the build machine's Mesa against the user's
#     hardware, and an amdgpu box handed an AppImage built on an nvidia one
#     gets a black window or a segfault inside the driver. The host's Vulkan
#     loader finds the host's ICDs under /usr/share/vulkan; ours would not.
#
#   Xlib and the xcb core (libX11, libX11-xcb, libxcb.so.1, libXau, libXdmcp,
#   libbsd, libmd)
#     Excluded not because they are unsafe in themselves but because libGL
#     above drags them in, and a bundled libxcb loaded next to a host libGL
#     that expects the host's is how you get two copies of one connection
#     object in the driver. libbsd and libmd are here only as libXdmcp's own
#     dependencies, and shipping the dependency of a library we did not ship
#     is the same mismatch one level down. The xcb *extension* libraries --
#     libxcb-xkb and friends -- are not excluded and are deliberately not
#     matched by a wildcard here: they hold no state, they only marshal one
#     protocol extension onto whichever libxcb.so.1 is loaded, and they are
#     not shipped by default on a minimal system the way libxcb.so.1 is.
#
#   libwayland-egl
#     The EGL platform shim, half of the driver pair above. libwayland-client
#     is not: it speaks the wire protocol, which is versioned and stable.
#
# Everything else is fair game, because it talks to files and sockets over
# formats and protocols that do not care what kernel is underneath:
# libxkbcommon today, and fontconfig, freetype, libwayland-client and
# libdbus-1 the day a build stops linking them statically -- which is where
# they are now, vendored into the binary, so the loop below never sees them.
HOST_ONLY='^(ld-linux.*|libc|libm|libdl|libpthread|librt|libresolv|libnsl|libutil|libanl|libBrokenLocale|libstdc\+\+|libgcc_s|libGL|libGLX|libGLdispatch|libGLESv[0-9]|libOpenGL|libEGL|libglapi|libgbm|libdrm|libvulkan|libX11|libX11-xcb|libxcb|libXau|libXdmcp|libbsd|libmd|libwayland-egl)\.so'

ldd "$APPDIR/usr/bin/dbdelve" |
  sed -n 's/^.* => \(\/[^ ]*\) (0x[0-9a-f]*)$/\1/p' |
  while read -r LIB; do
    SONAME="$(basename "$LIB")"
    if [[ "$SONAME" =~ $HOST_ONLY ]]; then
      echo "host   $SONAME"
    else
      # The soname, not the symlink target: the binary asks the loader for
      # libfreetype.so.6, and copying it as libfreetype.so.6.18.1 leaves
      # nothing at the name it looks for.
      install -m 644 "$(readlink -f "$LIB")" "$APPDIR/usr/lib/$SONAME"
      echo "bundle $SONAME"
    fi
  done

cat > "$APPDIR/AppRun" <<'APPRUN'
#!/bin/sh
# Appended, never replaced: a user with their own LD_LIBRARY_PATH keeps it, and
# the host's own directories stay ahead of nothing -- the bundled copies win
# for the names we ship and the host supplies every other name, which is the
# split package-appimage.sh made.
HERE="$(dirname "$(readlink -f "$0")")"
# A DBDelve started by another -- relaunching into its update, say -- inherits
# that one's LD_LIBRARY_PATH, which points into the old mount, so what the
# earlier AppRun saved is the original.
if [ -n "${DBDELVE_ORIGINAL_LD_LIBRARY_PATH+x}" ]; then
  export LD_LIBRARY_PATH="$DBDELVE_ORIGINAL_LD_LIBRARY_PATH"
elif [ -n "${DBDELVE_ORIGINAL_LD_LIBRARY_PATH_UNSET+x}" ]; then
  unset LD_LIBRARY_PATH
fi
unset DBDELVE_ORIGINAL_LD_LIBRARY_PATH DBDELVE_ORIGINAL_LD_LIBRARY_PATH_UNSET
# Saved for the ODBC bridge, whose vendor drivers must not see the bundled libraries.
if [ -n "${LD_LIBRARY_PATH+x}" ]; then
  export DBDELVE_ORIGINAL_LD_LIBRARY_PATH="$LD_LIBRARY_PATH"
else
  export DBDELVE_ORIGINAL_LD_LIBRARY_PATH_UNSET=1
fi
export LD_LIBRARY_PATH="$HERE/usr/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
exec "$HERE/usr/bin/dbdelve" "$@"
APPRUN
chmod 755 "$APPDIR/AppRun"

# aarch64 runners exist and the tool is per-architecture: appimagetool embeds a
# runtime binary for the architecture it was built for, so the x86_64 download
# on an arm runner produces an AppImage that cannot start.
# Kept out of linux/ on purpose: that directory is what the release workflow
# globs for assets, and a build tool sitting in it uploads itself.
TOOL="$TARGET_DIR/appimagetool-$ARCH.AppImage"
if [[ ! -x "$TOOL" ]]; then
  curl -fsSL -o "$TOOL" \
    "https://github.com/AppImage/appimagetool/releases/download/continuous/appimagetool-$ARCH.AppImage"
  chmod 755 "$TOOL"
fi

# --appimage-extract-and-run unconditionally: appimagetool is itself an
# AppImage, and mounting one needs FUSE, which a CI container does not have
# without --privileged. Unpacking to a temp dir costs a second and works
# everywhere. Note this is the *build* side only -- running the AppImage we
# produce still wants FUSE on the user's machine.
rm -f "$APPIMAGE"
ARCH="$ARCH" "$TOOL" --appimage-extract-and-run "$APPDIR" "$APPIMAGE"

if command -v sha256sum >/dev/null; then
  SHA="$(sha256sum "$APPIMAGE" | cut -d' ' -f1)"
else
  SHA="$(shasum -a 256 "$APPIMAGE" | cut -d' ' -f1)"
fi

echo "built $APPIMAGE (v$VERSION)"
echo "sha256 $SHA"
