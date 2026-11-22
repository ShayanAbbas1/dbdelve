{
  description = "DBDelve - A fast, native, multi-engine database client";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      rust-overlay,
    }:
    let
      inherit (nixpkgs) lib;

      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "aarch64-darwin"
      ];

      forAllSystems = lib.genAttrs systems;

      mkRustToolchain =
        pkgs: (rust-overlay.lib.mkRustBin { } pkgs).fromRustupToolchainFile ./rust-toolchain.toml;

      mkDbdelve =
        pkgs:
        let
          rustToolchain = mkRustToolchain pkgs;
          rustPlatform = pkgs.makeRustPlatform {
            cargo = rustToolchain;
            rustc = rustToolchain;
          };
        in
        pkgs.callPackage ./nix/package.nix {
          inherit rustPlatform;
        };
    in
    {
      packages = forAllSystems (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
        in
        {
          dbdelve = mkDbdelve pkgs;
          default = self.packages.${system}.dbdelve;
        }
      );

      overlays.default = final: _prev: {
        dbdelve = mkDbdelve final;
      };

      devShells = forAllSystems (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
          rustToolchain = mkRustToolchain pkgs;
          pkg = self.packages.${system}.dbdelve;
        in
        {
          default = pkgs.mkShell {
            packages = [
              rustToolchain
              pkgs.cmake
              pkgs.pkg-config
            ];

            buildInputs = [
              pkgs.sqlite
              pkgs.zstd
            ]
            ++ lib.optionals pkgs.stdenv.hostPlatform.isLinux pkg.passthru.linuxBuildLibs;

            env = lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux {
              LD_LIBRARY_PATH = "${lib.makeLibraryPath pkg.passthru.linuxRuntimeLibs}:/run/opengl-driver/lib";
            };
          };
        }
      );

      formatter = forAllSystems (system: nixpkgs.legacyPackages.${system}.nixfmt);
    };
}
