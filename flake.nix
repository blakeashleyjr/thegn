{
  description = "superzej — terminal-native git-worktree IDE on top of zellij";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    treefmt-nix.url = "github:numtide/treefmt-nix";
    treefmt-nix.inputs.nixpkgs.follows = "nixpkgs";
    # Toolchain with the wasm32-wasip1 target for the plugins.
    rust-overlay.url = "github:oxalica/rust-overlay";
    rust-overlay.inputs.nixpkgs.follows = "nixpkgs";
    # The pinned zellij superzej drives, on its OWN nixpkgs input so its version
    # is controlled independently of the main `nixpkgs` (which we bump for the
    # rust toolchain etc.). Currently locks to zellij 0.44.3 — the latest stable,
    # matching the plugins' `zellij-tile 0.44.x` ABI. Bump it deliberately with
    # `nix flake update nixpkgs-zellij` (and the plugin crates' zellij-tile to
    # match) — a routine `nix flake update` of the main nixpkgs never moves it.
    # NOT `follows = nixpkgs` precisely so the two stay decoupled.
    nixpkgs-zellij.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
  };

  outputs = {
    self,
    nixpkgs,
    flake-utils,
    treefmt-nix,
    rust-overlay,
    nixpkgs-zellij,
  }:
    flake-utils.lib.eachDefaultSystem (system: let
      pkgs = import nixpkgs {
        inherit system;
        overlays = [(import rust-overlay)];
      };
      treefmtEval = treefmt-nix.lib.evalModule pkgs ./treefmt.nix;
      # The pinned zellij superzej drives, from its own `nixpkgs-zellij` input so
      # its version is frozen in flake.lock independently of the main nixpkgs.
      # (zellij v0.44.x ships no flake.nix, so a standalone zellij flake input
      # would force a hand-rolled from-source build; a pinned nixpkgs is the
      # binary-cached way to control the exact version.)
      zellijPinned = (import nixpkgs-zellij {inherit system;}).zellij;
      superzej = pkgs.callPackage ./nix/package.nix {zellij = zellijPinned;};

      # One rust-overlay toolchain (clippy/rustfmt/rust-analyzer included) that
      # can also target wasm32-wasip1 — used for the dev shell and the plugins.
      rustToolchain = pkgs.rust-bin.stable.latest.default.override {
        targets = ["wasm32-wasip1"];
      };
      rustPlatformWasm = pkgs.makeRustPlatform {
        cargo = rustToolchain;
        rustc = rustToolchain;
      };
      mkPlugin = pname: subdir: wasmName:
        pkgs.callPackage ./nix/plugin.nix {
          rustPlatform = rustPlatformWasm;
          inherit pname wasmName;
          src = subdir;
        };
      sidebar = mkPlugin "superzej-sidebar" ./plugin/sidebar "sidebar.wasm";
      panel = mkPlugin "superzej-panel" ./plugin/panel "panel.wasm";
      tabbar = mkPlugin "superzej-tabbar" ./plugin/tabbar "tabbar.wasm";
      statusbar = mkPlugin "superzej-statusbar" ./plugin/statusbar "statusbar.wasm";
    in {
      packages.default = superzej;
      packages.superzej = superzej;
      # The pinned zellij superzej drives, exposed so dev (`just start*`) and
      # scripts can resolve its path: `nix build .#zellij`.
      packages.zellij = zellijPinned;
      packages.superzej-sidebar = sidebar;
      packages.superzej-panel = panel;
      packages.superzej-tabbar = tabbar;
      packages.superzej-statusbar = statusbar;

      # `nix fmt` formats every tracked file via treefmt.
      formatter = treefmtEval.config.build.wrapper;

      checks = {
        # `nix flake check` gates on a clean build, formatting, clippy, and the
        # two wasm plugins building.
        build = superzej;
        plugin-sidebar = sidebar;
        plugin-panel = panel;
        plugin-tabbar = tabbar;
        plugin-statusbar = statusbar;
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
        packages = with pkgs;
          [
            # rust toolchain (clippy/rustfmt/rust-analyzer + wasm32-wasip1 target)
            rustToolchain
            # task runner + formatter
            just
            treefmtEval.config.build.wrapper
            # linters
            shellcheck
            yamllint
            taplo
            # runtime tools superzej shells out to
            git
            fzf
            gum
            lazygit
            yazi
            delta
            gh
          ]
          # The same pinned zellij as the package, so `just start` runs the
          # version superzej ships — not whatever zellij is on the system.
          ++ [zellijPinned];
        shellHook = ''
          export PATH="$PWD/target/debug:$PATH"
          # Point dev superzej at the pinned zellij (the package wires this too).
          export SUPERZEJ_ZELLIJ_BIN="${zellijPinned}/bin/zellij"
          echo "superzej dev shell — 'cargo build', 'just build-plugins', 'just smoke', 'nix fmt'"
        '';
      };
    })
    // {
      # home-manager module. Imported as:
      #   imports = [ inputs.superzej.homeManagerModules.default ];
      homeManagerModules.default = import ./nix/hm-module.nix self;
    };
}
