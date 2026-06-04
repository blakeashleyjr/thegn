{
  description = "superzej — terminal-native git-worktree IDE on top of zellij";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    treefmt-nix.url = "github:numtide/treefmt-nix";
    treefmt-nix.inputs.nixpkgs.follows = "nixpkgs";
  };

  outputs = {
    self,
    nixpkgs,
    flake-utils,
    treefmt-nix,
  }:
    flake-utils.lib.eachDefaultSystem (system: let
      pkgs = nixpkgs.legacyPackages.${system};
      treefmtEval = treefmt-nix.lib.evalModule pkgs ./treefmt.nix;
      superzej = pkgs.callPackage ./nix/package.nix {};
    in {
      packages.default = superzej;
      packages.superzej = superzej;

      # `nix fmt` formats every tracked file via treefmt.
      formatter = treefmtEval.config.build.wrapper;

      checks = {
        # `nix flake check` gates on a clean build, formatting, and clippy.
        build = superzej;
        formatting = treefmtEval.config.build.check self;
        clippy = superzej.overrideAttrs (old: {
          pname = "superzej-clippy";
          nativeBuildInputs = (old.nativeBuildInputs or []) ++ [pkgs.clippy];
          buildPhase = "cargo clippy --all-targets --offline -- -D warnings";
          installPhase = "touch $out";
          doCheck = false;
          dontFixup = true;
        });
      };

      devShells.default = pkgs.mkShell {
        packages = with pkgs; [
          # rust toolchain
          cargo
          rustc
          clippy
          rustfmt
          rust-analyzer
          # task runner + formatter
          just
          treefmtEval.config.build.wrapper
          # linters
          shellcheck
          yamllint
          taplo
          # runtime tools superzej shells out to
          git
          zellij
          fzf
          gum
          lazygit
          yazi
          delta
        ];
        shellHook = ''
          export PATH="$PWD/target/debug:$PATH"
          echo "superzej dev shell — 'cargo build', 'just smoke', 'nix fmt'"
        '';
      };
    })
    // {
      # home-manager module. Imported as:
      #   imports = [ inputs.superzej.homeManagerModules.default ];
      homeManagerModules.default = import ./nix/hm-module.nix self;
    };
}
