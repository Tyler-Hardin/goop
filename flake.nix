{
  description = "goop — AI agent REPL";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs =
    {
      self,
      nixpkgs,
      flake-utils,
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        # CUDA is unfree — allow it so whisper.cpp can use GPU acceleration.
        pkgs = import nixpkgs {
          inherit system;
          config.allowUnfree = true;
        };
        isLinux = pkgs.stdenv.isLinux;

        # System libraries required by wry/webkit2gtk on Linux.
        # On macOS these are no-ops — wry uses the built-in WKWebView.
        # Everything else (libx11, libGL, etc.) is pulled in transitively.
        linuxBuildInputs = with pkgs; [
          atk
          cairo
          gdk-pixbuf
          glib
          gtk3
          libsoup_3
          pango
          webkitgtk_4_1
        ];

        # Hooks and tools needed at build-time on Linux so the webview
        # can find its GSettings schemas, typelibs, etc. at runtime.
        # makeWrapper is propagated by wrapGAppsHook3 — no need to list it.
        linuxNativeBuildInputs = with pkgs; [
          glib-networking
          wrapGAppsHook3
        ];

        # CUDA packages for GPU-accelerated whisper.cpp.
        # cuda_nvcc provides the CUDA compiler (needed by cmake at build time).
        # cuda_cudart, libcublas, and cuda_culibos provide the runtime libs
        # that whisper-rs links against.
        cudaNativeBuildInputs = with pkgs.cudaPackages; [
          cuda_nvcc
          cudatoolkit
        ];
        cudaBuildInputs = with pkgs.cudaPackages; [
          cuda_cudart
          libcublas
          pkgs.linuxPackages.nvidia_x11
        ];

        # Runtime deps for computer-use tools (shelled out via std::process::Command).
        # These are wrapped onto PATH so the goop binary can find them.
        linuxRuntimeDeps = with pkgs; [
          scrot
          tesseract
          xdotool
          which
          wmctrl
          xdg-utils
        ];
      in
      {
        packages.default = pkgs.rustPlatform.buildRustPackage {
          pname = "goop";
          version = "0.1.0";

          src = ./.;

          cargoLock = {
            lockFile = ./Cargo.lock;
          };

          # Enable CUDA feature so whisper-rs builds whisper.cpp with GPU support.
          cargoBuildFlags = [
            "--features"
            "cuda"
          ];

          # whisper-rs-sys tries to run bindgen (needs libclang) unless this is set.
          # The crate ships pre-generated bindings, so we skip the generation step.
          WHISPER_DONT_GENERATE_BINDINGS = "1";

          nativeBuildInputs = [
            pkgs.pkg-config
            pkgs.cmake
            pkgs.lld
            pkgs.trunk
            pkgs.wasm-bindgen-cli
            pkgs.binaryen
          ]
          ++ pkgs.lib.optionals isLinux linuxNativeBuildInputs
          ++ pkgs.lib.optionals isLinux cudaNativeBuildInputs;

          buildInputs = [
            pkgs.openssl
          ]
          ++ pkgs.lib.optionals isLinux linuxBuildInputs
          ++ pkgs.lib.optionals isLinux cudaBuildInputs;

          # Build the Leptos frontend with trunk before compiling the server.
          preBuild = ''
            export HOME="$TMPDIR/home"
            mkdir -p "$HOME/.config/goop"

            echo "=== trunk build ==="
            cd crates/goop-web

            # Trunk normally downloads wasm-bindgen and wasm-opt from GitHub,
            # but the Nix sandbox has no network.  Tell trunk to use the Nix-
            # provided versions and pre-populate its cache with symlinks.
            BINDGEN="$(command -v wasm-bindgen)"
            WASMOPT="$(command -v wasm-opt)"

            # Derive versions from the Nix binaries.
            BINDGEN_VER="$("$BINDGEN" --version | awk '{print $2}')"
            WASMOPT_VER="$("$WASMOPT" --version | awk '{print $3}')"

            export TRUNK_TOOLS_WASM_BINDGEN="$BINDGEN_VER"
            export TRUNK_TOOLS_WASM_OPT="$WASMOPT_VER"

            mkdir -p "$HOME/.cache/trunk/wasm-bindgen-$BINDGEN_VER"
            ln -sf "$BINDGEN" "$HOME/.cache/trunk/wasm-bindgen-$BINDGEN_VER/wasm-bindgen"

            mkdir -p "$HOME/.cache/trunk/wasm-opt-$WASMOPT_VER"
            ln -sf "$WASMOPT" "$HOME/.cache/trunk/wasm-opt-$WASMOPT_VER/wasm-opt"

            echo "Using wasm-bindgen $BINDGEN_VER, wasm-opt $WASMOPT_VER"

            RUSTFLAGS="-C linker=lld" trunk build --release --offline
            cd ../..
            echo "=== trunk done ==="
          '';

          # Copy the trunk dist alongside the binary.
          postInstall = ''
            mkdir -p "$out/dist"
            cp -r crates/goop-web/dist/* "$out/dist/"
          '';

          postFixup = pkgs.lib.optionalString isLinux ''
            wrapProgram $out/bin/goop \
              --prefix PATH : ${pkgs.lib.makeBinPath linuxRuntimeDeps}
          '';

          meta = with pkgs.lib; {
            description = "AI agent REPL with terminal and desktop GUI";
            license = licenses.mit;
            mainProgram = "goop";
          };
        };

        devShells.default = pkgs.mkShell {
          name = "goop-dev";

          inputsFrom = [ self.packages.${system}.default ];

          # wrapGAppsHook3 must be in nativeBuildInputs (not packages) so
          # its shell hook runs — this sets XDG_DATA_DIRS, GIO_EXTRA_MODULES,
          # GDK_PIXBUF_MODULE_FILE etc. that WebKitGTK needs at runtime.
          nativeBuildInputs =
            with pkgs;
            [
              glib-networking
              wrapGAppsHook3
            ]
            ++ pkgs.lib.optionals isLinux cudaNativeBuildInputs;

          buildInputs = pkgs.lib.optionals isLinux cudaBuildInputs;

          packages =
            with pkgs;
            [
              cargo
              clippy
              cmake
              lld
              rustc
              rustfmt
              rust-analyzer
              trunk
              wasm-bindgen-cli
              binaryen
            ]
            ++ pkgs.lib.optionals isLinux linuxRuntimeDeps;

          # whisper-rs build.rs hardcodes link-search paths for CUDA libs.
          # Point them at the nix store so the linker finds the actual libs.
          CUDA_PATH = pkgs.lib.optionalString isLinux "${pkgs.cudaPackages.cuda_nvcc}";

          # Skipping bindgen avoids needing libclang at build time.
          WHISPER_DONT_GENERATE_BINDINGS = "1";
        };

        formatter = pkgs.nixfmt-rfc-style;
      }
    );
}
