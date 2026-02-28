{
  description = "Elementium — Tauri-based Element Desktop replacement with native WebRTC";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, flake-utils, rust-overlay }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs { inherit system overlays; };

        rustToolchain = pkgs.rust-bin.stable.latest.default.override {
          extensions = [ "rust-src" "rust-analyzer" "clippy" ];
        };

        # Native libraries needed at build time
        nativeBuildInputs = with pkgs; [
          rustToolchain
          pkg-config
          gobject-introspection
          wrapGAppsHook3
          nodejs_22
          nodePackages.pnpm
          cargo-tauri
          just
          llvmPackages.clang
          llvmPackages.libclang
          mold
        ];

        # Libraries needed for linking
        buildInputs = with pkgs; [
          # Tauri / GTK / WebKit
          at-spi2-atk
          atkmm
          cairo
          dbus
          gdk-pixbuf
          glib
          gtk3
          harfbuzz
          librsvg
          libsoup_3
          openssl
          pango
          webkitgtk_4_1

          # Audio
          alsa-lib
          pipewire

          # Video
          libv4l
          libvpx
          libopus

          # Screen capture
          libx11
          libxrandr
          libxinerama
          libxcursor
          libxi
        ];

        # Libraries needed on LD_LIBRARY_PATH at runtime during dev
        runtimeLibs = with pkgs; [
          webkitgtk_4_1
          gtk3
          cairo
          gdk-pixbuf
          glib
          dbus
          openssl
          librsvg
          libsoup_3
          alsa-lib
          pipewire
          libvpx
          libopus
        ];

      in {
        devShells.default = pkgs.mkShell {
          inherit nativeBuildInputs buildInputs;

          shellHook = ''
            export LD_LIBRARY_PATH="${pkgs.lib.makeLibraryPath runtimeLibs}:$LD_LIBRARY_PATH"
            export XDG_DATA_DIRS="${pkgs.gsettings-desktop-schemas}/share/gsettings-schemas/${pkgs.gsettings-desktop-schemas.name}:${pkgs.gtk3}/share/gsettings-schemas/${pkgs.gtk3.name}:$XDG_DATA_DIRS"
            export GIO_MODULE_PATH="${pkgs.glib-networking}/lib/gio/modules"
            export LIBCLANG_PATH="${pkgs.llvmPackages.libclang.lib}/lib"
          '';
        };

        packages.default = pkgs.rustPlatform.buildRustPackage {
          pname = "elementium";
          version = "0.1.0";
          src = ./.;
          cargoLock.lockFile = ./Cargo.lock;

          inherit nativeBuildInputs buildInputs;

          # Will be filled in when we have a real build
          meta = with pkgs.lib; {
            description = "Tauri-based Element Desktop replacement with native WebRTC";
            license = licenses.agpl3Plus;
            platforms = platforms.linux;
          };
        };
      }
    );
}
