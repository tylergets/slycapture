{
  description = "slycapture X11 screen capture applet";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    crane.url = "github:ipetkov/crane";
  };

  outputs = {
    self,
    nixpkgs,
    flake-utils,
    crane,
  }:
    (flake-utils.lib.eachDefaultSystem (
      system: let
        pkgs = import nixpkgs {
          inherit system;
        };
        craneLib = crane.mkLib pkgs;
        src = pkgs.lib.cleanSourceWith {
          src = ./.;
          filter = craneLib.filterCargoSources;
        };

        commonArgs = {
          inherit src;
          strictDeps = true;
          nativeBuildInputs = with pkgs; [
            makeWrapper
            pkg-config
          ];
          buildInputs = with pkgs; [
            gtk3
            libappindicator-gtk3
            gsettings-desktop-schemas
          ];
        };

        # GSettings schemas for GTK widgets (the file chooser aborts without them).
        schemaDirs = with pkgs; [
          "${gsettings-desktop-schemas}/share/gsettings-schemas/${gsettings-desktop-schemas.name}"
          "${gtk3}/share/gsettings-schemas/${gtk3.name}"
        ];

        cargoArtifacts = craneLib.buildDepsOnly commonArgs;

        slycapture = craneLib.buildPackage (
          commonArgs
          // {
            inherit cargoArtifacts;
            pname = "slycapture";
            version = "0.2.0";
            meta = {
              mainProgram = "slycapture";
            };
            # wrapGAppsHook3 sets up XDG_DATA_DIRS (GSettings schemas, icons) for GTK.
            nativeBuildInputs = commonArgs.nativeBuildInputs ++ [pkgs.wrapGAppsHook3];
            preFixup = ''
              gappsWrapperArgs+=(
                --prefix LD_LIBRARY_PATH : "${pkgs.lib.makeLibraryPath [
                  pkgs.libappindicator-gtk3
                  pkgs.gtk3
                ]}" \
                --suffix PATH : "${pkgs.lib.makeBinPath [
                  pkgs.ffmpeg-full
                  pkgs.libnotify
                  pkgs.xdg-utils
                  pkgs.pulseaudio
                  pkgs.dunst
                  pkgs.i3
                ]}"
              )
            '';
          }
        );
      in {
        packages.default = slycapture;

        apps.default = flake-utils.lib.mkApp {
          drv = slycapture;
        };

        devShells.default = craneLib.devShell {
          packages = with pkgs; [
            rust-analyzer
            clippy
            rustfmt
            cargo-watch
            cargo-nextest
            cargo-edit
            cargo-audit
            cargo-deny
            cargo-expand
            libappindicator-gtk3
            gtk3
            pkg-config
            ffmpeg-full
          ];
          shellHook = ''
            export XDG_DATA_DIRS="${pkgs.lib.concatStringsSep ":" schemaDirs}:''${XDG_DATA_DIRS:-}"
          '';
        };
      }
    ))
    // {
      homeManagerModules.default = import ./nix/home-manager.nix {
        inherit self;
      };
    };
}
