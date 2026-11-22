{
  lib,
  stdenv,
  rustPlatform,
  addDriverRunpath,
  cmake,
  makeWrapper,
  pkg-config,
  sqlite,
  zstd,
  dbus,
  fontconfig,
  freetype,
  libglvnd,
  libx11,
  libxcb,
  libxkbcommon,
  unixodbc,
  vulkan-loader,
  wayland,
}:

let
  cargoToml = lib.importTOML ../Cargo.toml;
  version = cargoToml.workspace.package.version;

  linuxBuildLibs = [
    dbus
    fontconfig
    freetype
    libx11
    libxcb
    libxkbcommon
    unixodbc
    wayland
  ];

  linuxRuntimeLibs = [
    dbus
    fontconfig
    freetype
    libglvnd
    libx11
    libxcb
    libxkbcommon
    vulkan-loader
    wayland
  ];

  src = lib.fileset.toSource {
    root = ./..;
    fileset = lib.fileset.unions [
      ../Cargo.toml
      ../Cargo.lock
      ../build.rs
      ../src
      ../vendor
      ../plugins
      (lib.fileset.difference ../assets ../assets/screenshots)
      ../licenses
      ../LICENSE
      ../NOTICES.md
    ];
  };
in
rustPlatform.buildRustPackage {
  pname = "dbdelve";
  inherit version src;

  cargoLock = {
    lockFile = ../Cargo.lock;
  };

  # Build the app and the bundled ODBC bridge plugin in one pass.
  cargoBuildFlags = [
    "-p"
    "dbdelve"
    "-p"
    "dbdelve-plugin-odbc"
  ];

  # Tests require GPU/display support or build helper plugins on demand.
  doCheck = false;

  strictDeps = true;

  # The Nix Darwin sandbox cannot invoke Xcode's `xcrun metal` compiler;
  # compile Metal shaders at runtime instead.
  postPatch = lib.optionalString stdenv.hostPlatform.isDarwin /* bash */ ''
    substituteInPlace vendor/gpui-pre-apple/Cargo.toml \
      --replace-fail 'font-kit = [' 'font-kit = ["runtime_shaders",'
  '';

  nativeBuildInputs = [
    cmake
    pkg-config
  ]
  ++ lib.optionals stdenv.hostPlatform.isLinux [
    addDriverRunpath
  ]
  ++ lib.optionals stdenv.hostPlatform.isDarwin [
    makeWrapper
  ];

  buildInputs = [
    sqlite
    zstd
  ]
  ++ lib.optionals stdenv.hostPlatform.isLinux linuxBuildLibs;

  # CMake is invoked by bundled C/C++ build scripts (e.g. libduckdb-sys),
  # not as the top-level build system.
  dontUseCmakeConfigure = true;

  postInstall =
    if stdenv.hostPlatform.isLinux then
      /* bash */ ''
        mkdir -p "$out/libexec/dbdelve/plugins/odbc/bin"
        mv "$out/bin/dbdelve" "$out/libexec/dbdelve/dbdelve"
        mv "$out/bin/dbdelve-odbc" "$out/libexec/dbdelve/plugins/odbc/bin/dbdelve-odbc"
        install -Dm644 plugins/odbc/dbdelve-plugin.toml \
          "$out/libexec/dbdelve/plugins/odbc/dbdelve-plugin.toml"

        # `/proc/self/exe` resolves symlinks on Linux, so plugin discovery
        # still finds `plugins/odbc` beside `$out/libexec/dbdelve/dbdelve`.
        ln -s ../libexec/dbdelve/dbdelve "$out/bin/dbdelve"

        install -Dm644 assets/linux/dbdelve.desktop \
          "$out/share/applications/dbdelve.desktop"
        for size in 16 32 64 128 256 512; do
          install -Dm644 "assets/linux/icons/dbdelve-$size.png" \
            "$out/share/icons/hicolor/''${size}x''${size}/apps/dbdelve.png"
        done

        install -Dm644 LICENSE NOTICES.md -t "$out/share/doc/dbdelve"
        cp -R licenses "$out/share/doc/dbdelve/"
      ''
    else if stdenv.hostPlatform.isDarwin then
      /* bash */ ''
        app="$out/Applications/DBDelve.app"
        mkdir -p \
          "$app/Contents/MacOS" \
          "$app/Contents/Helpers" \
          "$app/Contents/Resources/plugins/odbc"

        mv "$out/bin/dbdelve" "$app/Contents/MacOS/DBDelve"
        mv "$out/bin/dbdelve-odbc" "$app/Contents/Helpers/dbdelve-odbc"
        install -Dm644 assets/macos/DBDelve.icns "$app/Contents/Resources/DBDelve.icns"
        install -Dm644 plugins/odbc/dbdelve-plugin.toml \
          "$app/Contents/Resources/plugins/odbc/dbdelve-plugin.toml"
        install -Dm644 NOTICES.md "$app/Contents/Resources/NOTICES.md"
        cp -R licenses "$app/Contents/Resources/"

        cat > "$app/Contents/Info.plist" <<EOF
        <?xml version="1.0" encoding="UTF-8"?>
        <!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
        <plist version="1.0">
        <dict>
        	<key>CFBundleName</key><string>DBDelve</string>
        	<key>CFBundleDisplayName</key><string>DBDelve</string>
        	<key>CFBundleIdentifier</key><string>com.shayanabbas.dbdelve</string>
        	<key>CFBundleExecutable</key><string>DBDelve</string>
        	<key>CFBundleIconFile</key><string>DBDelve</string>
        	<key>CFBundlePackageType</key><string>APPL</string>
        	<key>CFBundleShortVersionString</key><string>${version}</string>
        	<key>CFBundleVersion</key><string>${version}</string>
        	<key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
        	<key>LSMinimumSystemVersion</key><string>12.0</string>
        	<key>LSApplicationCategoryType</key><string>public.app-category.developer-tools</string>
        	<key>NSHighResolutionCapable</key><true/>
        </dict>
        </plist>
        EOF

        # Wrap rather than symlink so `_NSGetExecutablePath` resolves inside
        # the .app bundle and plugin discovery finds Contents/Resources/plugins.
        makeWrapper "$app/Contents/MacOS/DBDelve" "$out/bin/dbdelve"
      ''
    else
      "";

  postFixup = lib.optionalString stdenv.hostPlatform.isLinux /* bash */ ''
    # Set RUNPATH after fixupPhase's `patchelf --shrink-rpath` so
    # dlopen'd libraries (wayland, vulkan-loader, libglvnd, etc.) are
    # not stripped, without exporting LD_LIBRARY_PATH to child processes
    # (ssh, password commands, ODBC drivers).
    patchelf --add-rpath "${lib.makeLibraryPath linuxRuntimeLibs}" \
      "$out/libexec/dbdelve/dbdelve"
    addDriverRunpath "$out/libexec/dbdelve/dbdelve"
  '';

  passthru = {
    inherit linuxBuildLibs linuxRuntimeLibs;
  };

  meta = {
    description = cargoToml.package.description;
    homepage = "https://github.com/ShayanAbbas1/dbdelve";
    license = lib.licenses.gpl3Only;
    mainProgram = "dbdelve";
    platforms = lib.platforms.linux ++ lib.platforms.darwin;
  };
}
