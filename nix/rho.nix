{ craneLib }:
let
  src = craneLib.cleanCargoSource ../.;
  cargoToml = "${src}/bins/rho/Cargo.toml";
  crateInfo = craneLib.crateNameFromCargoToml { inherit cargoToml; };

  commonArgs = {
    inherit src;
    inherit (crateInfo) pname version;
    strictDeps = true;
  };

  withCargoArtifacts = commonArgs // {
    cargoArtifacts = craneLib.buildDepsOnly commonArgs;
  };
in
{
  package = craneLib.buildPackage withCargoArtifacts;

  checks = {
    clippy = craneLib.cargoClippy (withCargoArtifacts // {
      cargoClippyExtraArgs = "--workspace --all-targets --all-features -- -D warnings";
    });

    test = craneLib.cargoTest withCargoArtifacts;

    fmt = craneLib.cargoFmt commonArgs;
  };
}
