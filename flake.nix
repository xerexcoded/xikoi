{
  description = "Xikoi — a local-first neuromancer voice terminal";

  inputs = {
    # Current nixpkgs-unstable revision, verified on aarch64-darwin.
    nixpkgs.url = "github:NixOS/nixpkgs/9bc02893134c733dd85de46ee4fb2fac696b5529";

    # Nixpkgs unstable dropped Intel macOS in 26.11. The maintained 26.05
    # Darwin branch keeps that declared compatibility target buildable.
    nixpkgs-darwin-x86.url = "github:NixOS/nixpkgs/nixpkgs-26.05-darwin";

    # Supplies the official Rust release artifacts instead of a lagging
    # nixpkgs compiler package.
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      nixpkgs-darwin-x86,
      rust-overlay,
    }:
    let
      systems = [
        "aarch64-darwin"
        "x86_64-darwin"
        "aarch64-linux"
        "x86_64-linux"
      ];
      forAllSystems = nixpkgs.lib.genAttrs systems;
      manifest = builtins.fromTOML (builtins.readFile ./Cargo.toml);

      pkgsFor =
        system:
        import (if system == "x86_64-darwin" then nixpkgs-darwin-x86 else nixpkgs) {
          inherit system;
          overlays = [ rust-overlay.overlays.default ];
        };

      toolchainFor =
        pkgs:
        pkgs.rust-bin.stable."1.97.1".default.override {
          extensions = [
            "rust-src"
            "rustfmt"
            "clippy"
            "rust-analyzer"
          ];
        };

      packageFor =
        system:
        let
          pkgs = pkgsFor system;
          toolchain = toolchainFor pkgs;
          rustPlatform = pkgs.makeRustPlatform {
            cargo = toolchain;
            rustc = toolchain;
          };
        in
        rustPlatform.buildRustPackage {
          pname = manifest.package.name;
          version = manifest.package.version;
          src = pkgs.lib.cleanSource ./.;

          cargoLock.lockFile = ./Cargo.lock;
          strictDeps = true;

          nativeBuildInputs = pkgs.lib.optionals pkgs.stdenv.isLinux [ pkgs.pkg-config ];
          buildInputs = pkgs.lib.optionals pkgs.stdenv.isLinux [ pkgs.alsa-lib ];
          SSL_CERT_FILE = "${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt";

          meta = {
            description = manifest.package.description;
            mainProgram = "xikoi";
            platforms = systems;
          };
        };
    in
    {
      packages = forAllSystems (
        system:
        let
          package = packageFor system;
        in
        {
          default = package;
          xikoi = package;
        }
      );

      apps = forAllSystems (
        system:
        let
          pkgs = pkgsFor system;
          package = packageFor system;
          app = {
            type = "app";
            program = "${package}/bin/xikoi";
            meta.description = manifest.package.description;
          };
          voiceServer = pkgs.writeShellApplication {
            name = "xikoi-voice-server";
            runtimeInputs = [
              pkgs.uv
              pkgs.python312
            ];
            text = ''
              if [ "$(uname -s)" != "Darwin" ] || [ "$(uname -m)" != "arm64" ]; then
                echo "MLX-Audio requires Apple Silicon macOS." >&2
                exit 1
              fi
              exec uv run --frozen --project . mlx_audio.server --host 127.0.0.1 --port 8000 "$@"
            '';
          };
        in
        {
          default = app;
          xikoi = app;
          voice-server = {
            type = "app";
            program = "${voiceServer}/bin/xikoi-voice-server";
            meta.description = "Run Xikoi's pinned local MLX-Audio inference server";
          };
        }
      );

      devShells = forAllSystems (
        system:
        let
          pkgs = pkgsFor system;
          toolchain = toolchainFor pkgs;
        in
        {
          default = pkgs.mkShell {
            strictDeps = true;

            packages = [
              toolchain
              pkgs.cargo-edit
              pkgs.cargo-nextest
              pkgs.cargo-audit
              pkgs.nil
              pkgs.nixfmt
              pkgs.curl
              pkgs.jq
            ];

            nativeBuildInputs = pkgs.lib.optionals pkgs.stdenv.isLinux [ pkgs.pkg-config ];
            buildInputs = pkgs.lib.optionals pkgs.stdenv.isLinux [ pkgs.alsa-lib ];

            RUST_SRC_PATH = "${toolchain}/lib/rustlib/src/rust/library";
            RUST_BACKTRACE = "1";
          };

          voice = pkgs.mkShell {
            strictDeps = true;

            packages = [
              toolchain
              pkgs.cargo-edit
              pkgs.cargo-nextest
              pkgs.nil
              pkgs.nixfmt
              pkgs.uv
              pkgs.python312
              pkgs.curl
              pkgs.jq
            ];

            nativeBuildInputs = pkgs.lib.optionals pkgs.stdenv.isLinux [ pkgs.pkg-config ];
            buildInputs = pkgs.lib.optionals pkgs.stdenv.isLinux [ pkgs.alsa-lib ];

            UV_PYTHON = "${pkgs.python312}/bin/python";
            UV_PYTHON_DOWNLOADS = "never";
            RUST_SRC_PATH = "${toolchain}/lib/rustlib/src/rust/library";
            RUST_BACKTRACE = "1";
          };
        }
      );

      formatter = forAllSystems (system: (pkgsFor system).nixfmt);

      checks = forAllSystems (system: {
        xikoi = packageFor system;
      });
    };
}
