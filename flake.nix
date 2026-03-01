{
  description = "rho, an agent harness";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    crane.url = "github:ipetkov/crane";
  };

  outputs =
    { self, nixpkgs, flake-utils, rust-overlay, crane }:
    let
      mkRhoFor = pkgs:
        let
          rustToolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
          craneLib = (crane.mkLib pkgs).overrideToolchain rustToolchain;
        in
        import ./nix/rho.nix {
          inherit craneLib;
          src = craneLib.cleanCargoSource ./.;
        };
    in
    {
      overlays.default = nixpkgs.lib.composeExtensions (import rust-overlay) (
        final: _: { rho = (mkRhoFor final).package; }
      );
    }
    // flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ (import rust-overlay) ];
        };
        rustToolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
        rustNightlyToolchain = pkgs.rust-bin.selectLatestNightlyWith (toolchain: toolchain.default);
        rho = mkRhoFor pkgs;
      in
      {
        packages = {
          rho = rho.package;
          default = rho.package;
        };

        apps.default = flake-utils.lib.mkApp { drv = rho.package; };

        checks = { build = rho.package; } // rho.checks;

        formatter = pkgs.nixfmt;

        devShells.default = pkgs.mkShell {
          packages = with pkgs; [
            ripgrep
            git
            just
            mdbook

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
