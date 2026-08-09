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
          pnpm
          # Browser-driving tests: a real Chromium is the only way to exercise the
          # libwebrtc receive path (NetEq concealment, livekit's E2EE worker) that the
          # native Rust client cannot stand in for.
          playwright-driver.browsers
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

          # System tray
          libayatana-appindicator

          # Audio
          alsa-lib
          pipewire

          # Video
          libv4l
          libvpx
          libopus
          libjpeg

          # Hardware video encoding. VAAPI is the Linux interface to whatever the GPU
          # offers -- H.264, HEVC and AV1 on current AMD and Intel parts, none of which
          # includes VP8. `libva-utils` provides `vainfo`, which is how a human checks what
          # the machine can do without running the application.
          libva
          libva-utils

          # A reference decoder, for checking what our own encoder produces. Asserting the
          # NAL types in a bitstream proves it is shaped like H.264; only a decoder that
          # reconstructs the picture proves it *is* H.264. Not linked against -- it is used
          # by tests as a command.
          ffmpeg

          # GStreamer (needed by WebKitGTK for media playback)
          gst_all_1.gstreamer
          gst_all_1.gst-plugins-base
          gst_all_1.gst-plugins-good

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
          libayatana-appindicator
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
          libjpeg
          gst_all_1.gstreamer
          gst_all_1.gst-plugins-base
          gst_all_1.gst-plugins-good
        ];

      in {
        devShells.default = pkgs.mkShell {
          inherit nativeBuildInputs buildInputs;

          shellHook = ''
            export LD_LIBRARY_PATH="${pkgs.lib.makeLibraryPath runtimeLibs}:$LD_LIBRARY_PATH"
            export XDG_DATA_DIRS="${pkgs.gsettings-desktop-schemas}/share/gsettings-schemas/${pkgs.gsettings-desktop-schemas.name}:${pkgs.gtk3}/share/gsettings-schemas/${pkgs.gtk3.name}:$XDG_DATA_DIRS"
            export GIO_EXTRA_MODULES="${pkgs.glib-networking}/lib/gio/modules"
            export LIBCLANG_PATH="${pkgs.llvmPackages.libclang.lib}/lib"
            # Use the nix-provided browsers; playwright must not download its own.
            export PLAYWRIGHT_BROWSERS_PATH="${pkgs.playwright-driver.browsers}"
            export PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD=1
            export GST_PLUGIN_PATH="${pkgs.gst_all_1.gst-plugins-base}/lib/gstreamer-1.0:${pkgs.gst_all_1.gst-plugins-good}/lib/gstreamer-1.0''${GST_PLUGIN_PATH:+:$GST_PLUGIN_PATH}"
          '';
        };

        # Run a snapshot of Elementium: `nix run`
        #
        # Why a snapshot runner rather than a hermetic package. Tauri embeds the frontend
        # into the binary at build time, and this project's frontend is `element-web-dist`
        # -- 134MB, gitignored, produced by `scripts/prepare-build.sh`, which downloads an
        # Element Web release and builds the shims with pnpm. A `nix build` that produced it
        # would need that download pinned as a fixed-output derivation with a hash that
        # changes on every Element Web bump, which is real work and a separate change.
        #
        # What this gives instead is the thing the snapshot is *for*: a build frozen at a
        # moment, runnable while the working tree keeps moving. Snapshots live outside the
        # repo, so `git checkout`, a rebuild, or an unfinished refactor cannot disturb one
        # that is already running.
        apps.default = {
          type = "app";
          program = "${pkgs.writeShellApplication {
            name = "elementium-snapshot";
            runtimeInputs = [ pkgs.coreutils ];
            text = ''
              root="''${ELEMENTIUM_SNAPSHOTS:-$HOME/.local/share/elementium/snapshots}"
              latest="$root/latest"
              if [ ! -x "$latest/elementium" ]; then
                echo "No Elementium snapshot found in $root." >&2
                echo "Build one with:  nix run .#snapshot" >&2
                exit 1
              fi
              echo "Running snapshot: $(readlink -f "$latest")" >&2
              # The snapshot is a bare, unwrapped binary, so it finds none of its dynamic
              # libraries on its own -- it worked at all only from inside the dev shell,
              # which is the one environment a frozen snapshot should not need. Without
              # this it dies loading libayatana-appindicator before the window exists.
              export LD_LIBRARY_PATH="${pkgs.lib.makeLibraryPath runtimeLibs}''${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
              export XDG_DATA_DIRS="${pkgs.gsettings-desktop-schemas}/share/gsettings-schemas/${pkgs.gsettings-desktop-schemas.name}:${pkgs.gtk3}/share/gsettings-schemas/${pkgs.gtk3.name}''${XDG_DATA_DIRS:+:$XDG_DATA_DIRS}"
              # TLS for the WebKit network stack: without it every https request fails.
              export GIO_EXTRA_MODULES="${pkgs.glib-networking}/lib/gio/modules"
              exec "$latest/elementium" "$@"
            '';
          }}/bin/elementium-snapshot";
        };

        # Build a new snapshot: `nix run .#snapshot`
        #
        # Runs the ordinary release build inside the dev shell, then copies the result out
        # of the tree. Copied rather than symlinked on purpose: a symlink into `target/`
        # would make the "snapshot" change under the user the next time anything is rebuilt,
        # which is exactly what it exists to prevent.
        apps.snapshot = {
          type = "app";
          program = "${pkgs.writeShellApplication {
            name = "elementium-take-snapshot";
            runtimeInputs = [ pkgs.coreutils pkgs.nix pkgs.git ];
            text = ''
              repo="''${ELEMENTIUM_REPO:-$PWD}"
              if [ ! -f "$repo/src-tauri/tauri.conf.json" ]; then
                echo "Run this from the Elementium checkout, or set ELEMENTIUM_REPO." >&2
                exit 1
              fi
              root="''${ELEMENTIUM_SNAPSHOTS:-$HOME/.local/share/elementium/snapshots}"
              stamp="$(date -u +%Y%m%dT%H%M%SZ)"
              rev="$(git -C "$repo" rev-parse --short HEAD 2>/dev/null || echo nogit)"
              dest="$root/$stamp-$rev"

              echo "Building a release snapshot from $repo ($rev)..." >&2
              # `--no-bundle`: a snapshot wants the binary, and bundling additionally builds
              # an AppImage, which fails on NixOS because linuxdeploy expects
              # /usr/bin/xdg-open. The binary is what gets run here either way.
              ( cd "$repo" && nix develop -c cargo tauri build --no-bundle )

              binary="$repo/target/release/elementium"
              if [ ! -x "$binary" ]; then
                echo "The build did not produce $binary." >&2
                exit 1
              fi
              mkdir -p "$dest"
              cp "$binary" "$dest/elementium"
              git -C "$repo" rev-parse HEAD > "$dest/REVISION" 2>/dev/null || true
              ln -sfn "$dest" "$root/latest"
              echo "Snapshot ready: $dest" >&2
              echo "Run it with:    nix run" >&2
            '';
          }}/bin/elementium-take-snapshot";
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
