{
  description = "Consumer repo using shared base + rust precommit system";

  inputs = {
    precommit.url = "github:FredSystems/pre-commit-checks";
    nixpkgs.url = "github:nixos/nixpkgs/nixos-unstable";
  };

  outputs =
    {
      self,
      precommit,
      nixpkgs,
      ...
    }:
    let
      systems = precommit.lib.supportedSystems;
    in
    {
      ##########################################################################
      ## CHECKS — unified base+rust via mkCheck
      ##########################################################################
      checks = builtins.listToAttrs (
        map (system: {
          name = system;
          value = {
            pre-commit-check = precommit.lib.mkCheck {
              inherit system;
              src = ./.;
              check_rust = true;
              enableXtask = false;
              extraExcludes = [
                "typos.toml"
              ];
            };
          };
        }) systems
      );

      ##########################################################################
      ## DEV SHELLS — merged env + your extra Rust goodies
      ##########################################################################
      devShells = builtins.listToAttrs (
        map (system: {
          name = system;

          value =
            let
              pkgs = import nixpkgs { inherit system; };

              # Unified check result (base + rust)
              chk = self.checks.${system}."pre-commit-check";

              # Packages that git-hooks.nix / mkCheck say we need
              corePkgs = chk.enabledPackages or [ ];

              # Extra Rust / tooling packages (NO rustc/cargo/clippy here — those
              # come from extraDev's unified toolchain. cargo-deny / cargo-machete /
              # cargo-make are standalone binaries from nixpkgs and won't shadow
              # the unified toolchain as long as extraDev appears first in
              # buildInputs below.)
              extraRustTools = [
                pkgs.cargo-deny
                pkgs.cargo-machete
                pkgs.cargo-make
                pkgs.markdownlint-cli2
              ];

              # Extra dev packages provided by mkCheck (includes rustToolchain).
              # MUST appear first in buildInputs so its cargo/clippy/rustc
              # outrank the older versions transitively pulled in by the
              # nixpkgs cargo-* helpers above. Otherwise cargo and clippy end
              # up on different rustc versions and you get E0514 on every
              # `cargo clippy` after a `cargo build`.
              extraDev = chk.passthru.devPackages or [ ];

              # Library path packages: whatever mkCheck wants + your GL/Wayland bits
              libPkgs = chk.passthru.libPath or [ ];
            in
            {
              default = pkgs.mkShell {
                buildInputs = extraDev ++ corePkgs ++ extraRustTools;

                LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath libPkgs;

                shellHook = ''
                  ${chk.shellHook}

                  alias pre-commit="pre-commit run --all-files"
                '';
              };
            };
        }) systems
      );
    };
}
