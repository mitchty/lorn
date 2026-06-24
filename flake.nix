{
  description = "lorn";
  outputs =
    { self, ... }@inputs:
    inputs.flake-utils.lib.eachDefaultSystem (
      system:
      let
        metaCommon = desc: {
          description = if desc == "" then "lorn" else "lorn " + desc;
          mainProgram = "lorn";
        };

        stableRust = (
          inputs.fenix.packages.${system}.stable.withComponents [
            "cargo"
            "clippy"
            "llvm-tools"
            "rustc"
            "rust-src"
            "rustfmt"
            "rust-analyzer"
          ]
        );

        pkgs = import inputs.nixpkgs {
          inherit system;
          overlays = [
            inputs.fenix.overlays.default
            (self: super: {
              apple-sdk-test = super.apple-sdk;
            })
          ];
        };

        # Static musl Linux binary for current arch. aarch64 out of scope for now.
        pkgsMusl = import inputs.nixpkgs {
          inherit system;
          overlays = [ inputs.fenix.overlays.default ];
          crossSystem = {
            config = "${pkgs.stdenv.hostPlatform.parsed.cpu.name}-unknown-linux-musl";
          };
        };

        pkgsDarwin =
          if pkgs.stdenv.isDarwin then
            import inputs.nixpkgs {
              inherit system;
              overlays = [ inputs.fenix.overlays.default ];
              crossSystem = pkgs.stdenv.hostPlatform;
            }
          else
            null;

        # Windows cross-compile via mingw64, only works on linux.
        pkgsWindows = import inputs.nixpkgs {
          inherit system;
          overlays = [ inputs.fenix.overlays.default ];
          crossSystem = {
            config = "x86_64-w64-mingw32";
            libc = "msvcrt";
          };
        };

        inherit (pkgs) lib;

        craneLib = inputs.crane.mkLib pkgs;

        craneLibMusl =
          let
            muslTarget = "${pkgs.stdenv.hostPlatform.parsed.cpu.name}-unknown-linux-musl";
          in
          (inputs.crane.mkLib pkgsMusl).overrideToolchain (
            p:
            p.fenix.combine [
              p.fenix.stable.rustc
              p.fenix.stable.cargo
              p.fenix.targets.${muslTarget}.stable.rust-std
            ]
          );

        craneLibDarwin =
          if pkgs.stdenv.isDarwin then
            (inputs.crane.mkLib pkgsDarwin).overrideToolchain (
              p:
              p.fenix.combine [
                p.fenix.stable.rustc
                p.fenix.stable.cargo
                p.fenix.stable.rust-std
              ]
            )
          else
            null;

        craneLibWindows = (inputs.crane.mkLib pkgsWindows).overrideToolchain (
          p:
          p.fenix.combine [
            p.fenix.stable.rustc
            p.fenix.stable.cargo
            p.fenix.targets.x86_64-pc-windows-gnu.stable.rust-std
          ]
        );

        srcDeps = lib.fileset.toSource {
          root = ./.;
          fileset = lib.fileset.unions [
            ./Cargo.lock
            ./Cargo.toml
            (lib.fileset.fileFilter (file: file.hasExt "toml") ./crates)
            (lib.fileset.fileFilter (file: file.name == "build.rs") ./crates)
          ];
        };

        src = lib.fileset.toSource {
          root = ./.;
          fileset = lib.fileset.unions [
            (lib.fileset.fileFilter (file: file.hasExt "rs") ./crates)
            (lib.fileset.fileFilter (file: file.hasExt "toml") ./.)
            ./Cargo.lock
          ];
        };

        treefmtEval = inputs.treefmt-nix.lib.evalModule pkgs {
          projectRootFile = "flake.nix";
          programs = {
            nixfmt.enable = true;
            rustfmt = {
              enable = true;
              edition = "2024";
            };
            taplo.enable = true;
          };
        };

        hookTools = with pkgs; {
          inherit
            taplo
            nixfmt
            rustfmt
            git
            nix
            treefmt
            ;
        };

        git-hooks-check = inputs.git-hooks.lib.${system}.run {
          src = ./.;
          tools = hookTools;
          hooks = {
            nix-flake-check = {
              enable = true;
              name = "nix-flake-check";
              entry = "${pkgs.nix}/bin/nix flake check -L";
              language = "system";
              pass_filenames = false;
              stages = [ "pre-push" ];
            };
            treefmt.enable = false;
          };
        };

        commonArgs = {
          inherit src;
          strictDeps = true;
          nativeBuildInputs = [ pkgs.git ];
          buildInputs =
            with pkgs;
            [ ]
            ++ lib.optionals pkgs.stdenv.hostPlatform.isLinux [
              pkgs.mold
              pkgs.lld
            ]
            ++ lib.optionals pkgs.stdenv.isDarwin [ ];
        };

        commonArgsMusl = {
          inherit src;
          strictDeps = true;
          nativeBuildInputs = [ pkgsMusl.git ];
          buildInputs = [ ];
          CARGO_BUILD_TARGET = "${pkgs.stdenv.hostPlatform.parsed.cpu.name}-unknown-linux-musl";
          CARGO_BUILD_RUSTFLAGS = "-C target-feature=+crt-static -C link-arg=-static";
        };

        commonArgsDarwin =
          if pkgs.stdenv.isDarwin then
            {
              inherit src;
              strictDeps = true;
              nativeBuildInputs = [ pkgsDarwin.git ];
              buildInputs = with pkgsDarwin; [ apple-sdk ];
            }
          else
            { };

        commonArgsWindows =
          let
            buildPlatformSuffix = lib.strings.toLower pkgs.pkgsBuildHost.stdenv.hostPlatform.rust.cargoEnvVarTarget;
          in
          {
            inherit src;
            strictDeps = true;
            nativeBuildInputs = with pkgs; [
              git
              buildPackages.nasm
              buildPackages.cmake
            ];
            buildInputs = with pkgsWindows.windows; [ pthreads ];
            CARGO_BUILD_TARGET = "x86_64-pc-windows-gnu";
            CFLAGS = "-Wno-stringop-overflow -Wno-array-bounds -Wno-restrict";
            CFLAGS_x86_64-pc-windows-gnu = "-I${pkgsWindows.windows.pthreads}/include";
            "CC_${buildPlatformSuffix}" = "cc";
            "CXX_${buildPlatformSuffix}" = "c++";
          };

        nixEnvArgs = {
          NIX_GIT_REV = version;
        };

        devArgs = {
          CARGO_PROFILE = "dev";
        };

        releaseArgs = {
          CARGO_PROFILE = "release";
          RUSTFLAGS = "-D warnings";
        };

        cargoArtifacts = craneLib.buildDepsOnly (commonArgs // nixEnvArgs // devArgs // { src = srcDeps; });

        cargoArtifactsMusl = craneLibMusl.buildDepsOnly (commonArgsMusl // { src = srcDeps; });

        cargoArtifactsDarwin =
          if pkgs.stdenv.isDarwin then
            craneLibDarwin.buildDepsOnly (commonArgsDarwin // { src = srcDeps; })
          else
            null;

        cargoArtifactsWindows = craneLibWindows.buildDepsOnly (commonArgsWindows // { src = srcDeps; });

        version = self.rev or self.dirtyShortRev or "nix-flake-cant-get-git-commit-sha";

        individualCrateArgs = commonArgs // {
          inherit cargoArtifacts;
          doCheck = false;
        };

        fileSetForCrate =
          crate:
          lib.fileset.toSource {
            root = ./.;
            fileset = lib.fileset.unions [
              ./Cargo.toml
              ./Cargo.lock
              (craneLib.fileset.commonCargoSources crate)
              (lib.fileset.fileFilter (file: file.hasExt "rs") ./crates)
              (lib.fileset.maybeMissing ./crates/${crate}/Cargo.toml)
              (lib.fileset.maybeMissing ./crates/${crate}/build.rs)
            ];
          };

        lorn = craneLib.buildPackage (
          individualCrateArgs
          // nixEnvArgs
          // devArgs
          // {
            pname = "lorn";
            cargoExtraArgs = "-p lorn";
            src = fileSetForCrate ./crates/lorn;
          }
        );

        lorn-lto = craneLib.buildPackage (
          individualCrateArgs
          // nixEnvArgs
          // releaseArgs
          // {
            pname = "lorn";
            cargoExtraArgs = "-p lorn";
            src = fileSetForCrate ./crates/lorn;
          }
        );

        lorn-release-linux = craneLibMusl.buildPackage (
          commonArgsMusl
          // {
            pname = "lorn-release";
            version = version;
            cargoArtifacts = cargoArtifactsMusl;
            cargoExtraArgs = "-p lorn";
            src = fileSetForCrate ./crates/lorn;
            NIX_GIT_REV = version;
            doCheck = false;
            meta = metaCommon "release static linux build" // {
              platforms = [
                "x86_64-linux"
                "aarch64-linux"
              ];
            };
          }
        );

        lorn-release-darwin =
          if pkgs.stdenv.isDarwin then
            craneLibDarwin.buildPackage (
              commonArgsDarwin
              // {
                pname = "lorn-release";
                version = version;
                cargoArtifacts = cargoArtifactsDarwin;
                cargoExtraArgs = "-p lorn";
                src = fileSetForCrate ./crates/lorn;
                NIX_GIT_REV = version;
                doCheck = false;
                postInstall = ''
                  for binary in $out/bin/*; do
                    libiconv_path=$(otool -L "$binary" | awk '/\/nix\/store.*libiconv/ {print $1}' || true)
                    if [ -n "$libiconv_path" ]; then
                      install_name_tool -change "$libiconv_path" /usr/lib/libiconv.2.dylib "$binary"
                    fi
                  done
                '';
                meta = metaCommon "release macos build" // {
                  platforms = [
                    "x86_64-darwin"
                    "aarch64-darwin"
                  ];
                };
              }
            )
          else
            null;

        lorn-release-windows = craneLibWindows.buildPackage (
          commonArgsWindows
          // {
            pname = "lorn-release";
            version = version;
            cargoArtifacts = cargoArtifactsWindows;
            cargoExtraArgs = "-p lorn";
            src = fileSetForCrate ./crates/lorn;
            NIX_GIT_REV = version;
            doCheck = false;
            meta = metaCommon "release windows x86_64 build";
          }
        );

      in
      {
        formatter = treefmtEval.config.build.wrapper;

        checks = {
          formatter = treefmtEval.config.build.check self;
          git-hooks = git-hooks-check;
          inherit lorn;

          lorn-clippy = craneLib.cargoClippy (
            commonArgs
            // nixEnvArgs
            // devArgs
            // {
              inherit cargoArtifacts;
              cargoClippyExtraArgs = "--all-targets -- --deny warnings";
            }
          );

          lorn-doc = craneLib.cargoDoc (
            commonArgs
            // nixEnvArgs
            // devArgs
            // {
              inherit cargoArtifacts;
              env.RUSTDOCFLAGS = "--deny warnings";
            }
          );

          lorn-nextest = craneLib.cargoNextest (
            commonArgs
            // nixEnvArgs
            // devArgs
            // {
              inherit cargoArtifacts;
              partitions = 1;
              partitionType = "count";
              cargoNextestPartitionsExtraArgs = "--no-tests=pass";
            }
          );
        };

        packages = {
          inherit
            lorn
            lorn-lto
            lorn-release-linux
            lorn-release-windows
            ;
          default = lorn;
          clippy = self.checks.${system}.lorn-clippy;
          doc = self.checks.${system}.lorn-doc;
          nextest = self.checks.${system}.lorn-nextest;
        }
        // lib.optionalAttrs pkgs.stdenv.isDarwin {
          lorn-release = lorn-release-darwin;
        };

        apps = {
          lorn = inputs.flake-utils.lib.mkApp { drv = lorn; };
          lorn-lto = inputs.flake-utils.lib.mkApp { drv = lorn-lto; };
          default = inputs.flake-utils.lib.mkApp { drv = lorn; };
          update = {
            type = "app";
            program = "${
              pkgs.writeShellApplication {
                name = "update";
                text = ''
                  set -e
                  nix flake update
                  cargo update --verbose
                  cargo upgrade --verbose
                '';
              }
            }/bin/update";
            meta = {
              description = "Update flake inputs and cargo dependencies";
              mainProgram = "update";
            };
          };
        }
        // lib.optionalAttrs pkgs.stdenv.isDarwin {
          lorn-release = inputs.flake-utils.lib.mkApp { drv = lorn-release-darwin; };
        };

        devShells.default = craneLib.devShell {
          checks = self.checks.${system};

          packages =
            (with pkgs; [
              cargo-edit
              cargo-outdated
              gitFull
              nil
              stableRust
              kubectl
              jq
            ])
            ++ (lib.attrValues hookTools)
            ++ commonArgs.buildInputs
            ++ commonArgs.nativeBuildInputs
            ++ lib.optionals pkgs.stdenv.isDarwin [ pkgs.apple-sdk-test ];

          shellHook = ''
            ${git-hooks-check.shellHook}
          '';

          RUST_SRC_PATH = "${stableRust}/lib/rustlib/src/rust/library";
        };
      }
    );

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";

    crane.url = "github:ipetkov/crane";

    flake-utils.url = "github:numtide/flake-utils";

    fenix.url = "github:nix-community/fenix";
    treefmt-nix.url = "github:numtide/treefmt-nix";

    git-hooks = {
      url = "github:cachix/git-hooks.nix";
      inputs.nixpkgs.follows = "nixpkgs";
    };

    advisory-db = {
      url = "github:rustsec/advisory-db";
      flake = false;
    };
  };
}
