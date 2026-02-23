{
  description = "rho, an agent harness";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    { self, nixpkgs, flake-utils, rust-overlay }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ (import rust-overlay) ];
        };
        rustToolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
        rustNightlyToolchain = pkgs.rust-bin.selectLatestNightlyWith (toolchain: toolchain.default);
      in
      {
        devShells.default = pkgs.mkShell {
          packages = with pkgs; [
            ripgrep
            git
            just

            # we need all of these, _and_ the order is important!
            rustup
            rustToolchain
            rustNightlyToolchain
          ];
          shellHook = ''
            rustup default stable >/dev/null 2>&1 || true
            rustup toolchain list | grep -q '^nightly' || rustup toolchain install nightly >/dev/null 2>&1 || true
          '';
        };
      }
    );
}
