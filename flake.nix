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
        pkgs = nixpkgs.legacyPackages.${system};
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

          nativeBuildInputs = [ pkgs.pkg-config ] ++ pkgs.lib.optionals isLinux linuxNativeBuildInputs;

          buildInputs = [ pkgs.openssl ] ++ pkgs.lib.optionals isLinux linuxBuildInputs;

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
          nativeBuildInputs = with pkgs; [
            glib-networking
            wrapGAppsHook3
          ];

          packages =
            with pkgs;
            [
              cargo
              clippy
              rustc
              rustfmt
              rust-analyzer
            ]
            ++ pkgs.lib.optionals isLinux linuxRuntimeDeps;
        };

        formatter = pkgs.nixfmt-rfc-style;
      }
    );
}
