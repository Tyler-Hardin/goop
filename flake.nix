{
  description = "goop — AI agent REPL";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = nixpkgs.legacyPackages.${system};
        isLinux = pkgs.stdenv.isLinux;

        # System libraries required by wry/webkit2gtk on Linux.
        # On macOS these are no-ops — wry uses the built-in WKWebView.
        linuxBuildInputs = with pkgs; [
          glib
          gtk3
          webkitgtk_4_1
          libsoup_3
          cairo
          pango
          atk
          gdk-pixbuf
          libx11
          libxcb
        ];

        # Hooks and tools needed at build-time on Linux so the webview
        # can find its GSettings schemas, typelibs, etc. at runtime.
        linuxNativeBuildInputs = with pkgs; [
          wrapGAppsHook3
          glib-networking
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

          nativeBuildInputs = [ pkgs.pkg-config ]
            ++ pkgs.lib.optionals isLinux linuxNativeBuildInputs;

          buildInputs = pkgs.lib.optionals isLinux linuxBuildInputs;

          meta = with pkgs.lib; {
            description = "AI agent REPL with terminal and desktop GUI";
            license = licenses.mit;
            mainProgram = "goop";
          };
        };

        devShells.default = pkgs.mkShell {
          name = "goop-dev";

          inputsFrom = [ self.packages.${system}.default ];

          packages = with pkgs; [
            cargo
            rustc
            rustfmt
            clippy
            rust-analyzer
          ] ++ pkgs.lib.optionals isLinux linuxBuildInputs
            ++ pkgs.lib.optionals isLinux linuxNativeBuildInputs;
        };
      });
}
