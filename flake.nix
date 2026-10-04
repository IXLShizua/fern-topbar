{
  description = "fern-topbar GTK panel with Niri integration";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    crane.url = "github:ipetkov/crane";

    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      nixpkgs,
      flake-utils,
      crane,
      rust-overlay,
      ...
    }:
    flake-utils.lib.eachSystem
      [
        "x86_64-linux"
        "aarch64-linux"
      ]
      (
        system:
        let
          pkgs = import nixpkgs {
            inherit system;
            overlays = [ (import rust-overlay) ];
          };

          rustToolchain = pkgs.rust-bin.stable.latest.default.override {
            extensions = [
              "rust-src"
              "rust-analyzer"
            ];
          };

          craneLib = (crane.mkLib pkgs).overrideToolchain (_: rustToolchain);

          src =
            let
              root = ./.;
              assets = ./assets;
            in
            pkgs.lib.fileset.toSource {
              inherit root;

              fileset = pkgs.lib.fileset.unions [
                (craneLib.fileset.commonCargoSources root)
                (pkgs.lib.fileset.maybeMissing assets)
                ./LICENSE
              ];
            };

          commonArgs = {
            inherit src;

            strictDeps = true;

            nativeBuildInputs = with pkgs; [
              pkg-config
              dart-sass
              wrapGAppsHook4
            ];

            buildInputs = with pkgs; [
              gtk4
              libadwaita
              gtk4-layer-shell
              libpulseaudio
            ];
          };

          cargoArtifacts = craneLib.buildDepsOnly commonArgs;

          fernTopbar = craneLib.buildPackage (
            commonArgs
            // {
              inherit cargoArtifacts;

              doCheck = false;

              postInstall = ''
                install -Dm644 LICENSE "$out/share/licenses/fern-topbar/LICENSE"
              '';

              meta = {
                description = "Configurable GTK panel with Niri integration";
                mainProgram = "fern-topbar";
                license = pkgs.lib.licenses.gpl3Plus;
                platforms = [
                  "x86_64-linux"
                  "aarch64-linux"
                ];
              };
            }
          );
        in
        {
          packages.default = fernTopbar;

          apps.default =
            flake-utils.lib.mkApp {
              drv = fernTopbar;
            }
            // {
              inherit (fernTopbar) meta;
            };

          checks = {
            package = fernTopbar;

            formatting = craneLib.cargoFmt {
              inherit src;
            };

            clippy = craneLib.cargoClippy (
              commonArgs
              // {
                inherit cargoArtifacts;

                cargoClippyExtraArgs = "--all-targets -- -D warnings";
              }
            );

            # GTK tests need a graphical session; sandbox checks only compile them.
            test-build = craneLib.cargoTest (
              commonArgs
              // {
                inherit cargoArtifacts;

                cargoTestExtraArgs = "--no-run";
              }
            );
          };

          formatter = pkgs.nixfmt;

          devShells.default = craneLib.devShell {
            inputsFrom = [ fernTopbar ];
          };
        }
      );
}
