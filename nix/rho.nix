{ craneLib, src }:
let
  crateInfo = craneLib.crateNameFromCargoToml { cargoToml = "${src}/bins/rho/Cargo.toml"; };

  commonArgs = {
    inherit src;
    inherit (crateInfo) pname version;
    strictDeps = true;
  };

  cargoArtifacts = craneLib.buildDepsOnly commonArgs;
in
{
  package = craneLib.buildPackage (commonArgs // {
    inherit cargoArtifacts;
  });

  checks = {
    clippy = craneLib.cargoClippy (commonArgs // {
      inherit cargoArtifacts;
      cargoClippyExtraArgs = "--workspace --all-targets --all-features -- -D warnings";
    });

    test = craneLib.cargoTest (commonArgs // {
      inherit cargoArtifacts;
    });

    fmt = craneLib.cargoFmt commonArgs;
  };
}
