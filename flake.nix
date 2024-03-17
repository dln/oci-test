{
  description = "OCI runtime experiment using youki";

  inputs = {
    nixpkgs.url = "nixpkgs/nixpkgs-unstable";
    rust-overlay.url = "github:oxalica/rust-overlay";
  };

  outputs = { self, nixpkgs, rust-overlay }:
    let
      overlays = [
        rust-overlay.overlays.default
        (final: prev: {
          rustToolchain = (final.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml).override { extensions = [ "rust-src"]; };
        })
      ];

      supportedSystems = [ "x86_64-linux" "aarch64-linux" ];
      forEachSupportedSystem = f: nixpkgs.lib.genAttrs supportedSystems (system: f rec {
        pkgs = import nixpkgs { inherit overlays system; };
        lib = pkgs.lib;
        devPkgs = with pkgs; [
          git
          go-task
          rust-analyzer
          rustToolchain
        ];
      });

    in 
    {
      packages = forEachSupportedSystem ({ pkgs, devPkgs, lib }: rec {
        default = builder;
        builder = pkgs.dockerTools.streamLayeredImage {
          name = "builder";
          tag = "latest";
          maxLayers = 13;
          contents = pkgs.buildEnv {
            name = "builder";
            paths = with pkgs; devPkgs ++ [
              bash
              coreutils-full
              dockerTools.usrBinEnv
              dockerTools.binSh
              dockerTools.caCertificates
              dockerTools.fakeNss
            ];
          };
          extraCommands = "mkdir -m 0777 tmp";
        };
      });

      devShells = forEachSupportedSystem ({ pkgs, devPkgs, ... }: {
        default = pkgs.mkShell {
          packages = devPkgs;
          env = {
            RUST_SRC_PATH = "${pkgs.rustToolchain}/lib/rustlib/src/rust/library";
          };
        };
      });
    };
}
