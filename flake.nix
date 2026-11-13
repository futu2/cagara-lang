{
  description = "Cagara: a small typed query language compiling to SQL";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs { inherit system; };
      in
      {
        devShells.default = pkgs.mkShell {
          packages = with pkgs; [
            cargo
            rustc
            clippy
            rustfmt
            rust-analyzer
            sqlite
            duckdb
            # The PostgreSQL differential test drives PGlite (Postgres in
            # WebAssembly) through node; install it once with
            # `npm install --no-save @electric-sql/pglite`.
            nodejs
          ];
          RUST_SRC_PATH = "${pkgs.rustPlatform.rustLibSrc}";
        };

        packages.default = pkgs.rustPlatform.buildRustPackage {
          pname = "cagara";
          version = "0.4.1";
          src = ./.;
          cargoLock.lockFile = ./Cargo.lock;
          cargoBuildFlags = [ "-p" "cagara-cli" ];
        };
      });
}
