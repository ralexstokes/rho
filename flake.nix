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
      rustOverlay = import rust-overlay;

      mkPkgs = system:
        import nixpkgs {
          inherit system;
          overlays = [ rustOverlay ];
        };

      mkRhoFor = pkgs:
        let
          rustToolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
          craneLib = (crane.mkLib pkgs).overrideToolchain rustToolchain;
        in
        import ./nix/rho.nix { inherit craneLib; };
    in
    {
      overlays.default = nixpkgs.lib.composeExtensions rustOverlay (
        final: _prev: { rho = (mkRhoFor final).package; }
      );
    }
    // flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = mkPkgs system;
        rho = mkRhoFor pkgs;
        package = rho.package;
        rustToolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
        rustNightlyToolchain = pkgs.rust-bin.selectLatestNightlyWith (toolchain: toolchain.default);
      in
      {
        packages = {
          default = package;
          rho = package;
        };

        apps.default = flake-utils.lib.mkApp { drv = package; };

        checks = rho.checks // { build = package; };

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
