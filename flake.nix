{
  description = "rho Rust development environment";

  inputs = {
    rust-env.url = "github:ralexstokes/rust-nix-template";
  };

  outputs =
    { self, rust-env }:
    rust-env.lib.mkRustProject {
      src = ./.;
      # The check sandbox keeps only Cargo sources by default; anything else a
      # check reads (configs, docs pulled in by include_str!) must be listed
      # here or `nix flake check` silently runs without it.
      extraSourceFilter =
        path: type:
        builtins.match ".*/\\.config/nextest\\.toml" (toString path) != null
        || builtins.match ".*/clippy\\.toml" (toString path) != null
        || builtins.match ".*/tools/check-pure-deps" (toString path) != null;
      extraCiCommands = ''
        bash ./tools/check-pure-deps
      '';
      # Escape hatches (see the template repo's README):
      #   projectName = "rho";
      #   extraShellPackages = pkgs: [ pkgs.mdbook ];
      #   extraChecks = pkgs: { };
    };
}
