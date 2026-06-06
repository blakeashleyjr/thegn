{
  description = "superzej — terminal-native git-worktree IDE on top of zellij";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    flake-utils.url = "github:numtide/flake-utils";
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
    # The pinned yazi superzej drives for its bottom file-manager drawer, on its
    # OWN nixpkgs input — same rationale as `nixpkgs-zellij`: superzej bundles a
    # specific yazi (+ its preview tools) independent of the user's system and of
    # the main `nixpkgs`. Bump it deliberately with `nix flake update nixpkgs-yazi`.
    nixpkgs-yazi.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
  };

  outputs = {
    self,
    nixpkgs,
    flake-utils,
    rust-overlay,
    nixpkgs-zellij,
    nixpkgs-yazi,
  }:
    flake-utils.lib.eachDefaultSystem (system: let
      pkgs = import nixpkgs {
        inherit system;
        overlays = [(import rust-overlay)];
      };
      # Formatter binaries that treefmt.toml references.  Bundled together so
      # both the `formatter` wrapper and `checks.formatting` use identical
      # versions — no drift between `nix fmt` and the devenv pre-commit hook.
      fmtPackages = with pkgs; [
        # Use pkgs.rustfmt (not rustToolchain) so the formatter version tracks
        # nixpkgs-unstable, independent of the rust-overlay pin.
        rustfmt
        alejandra
        shfmt
        taplo
        yamlfmt
        prettier
      ];
      # `nix fmt` wrapper: reads treefmt.toml from the source tree, with all
      # formatter binaries pre-wired onto PATH.
      treefmtWrapper = pkgs.writeShellScriptBin "treefmt" ''
        export PATH="${pkgs.lib.makeBinPath fmtPackages}:$PATH"
        exec ${pkgs.treefmt}/bin/treefmt \
          --config-file="$(${pkgs.git}/bin/git rev-parse --show-toplevel)/treefmt.toml" \
          "$@"
      '';
      # The pinned zellij superzej drives, from its own `nixpkgs-zellij` input so
      # its version is frozen in flake.lock independently of the main nixpkgs.
      # (zellij v0.44.x ships no flake.nix, so a standalone zellij flake input
      # would force a hand-rolled from-source build; a pinned nixpkgs is the
      # binary-cached way to control the exact version.)
      zellijPinned = (import nixpkgs-zellij {inherit system;}).zellij;
      # The pinned yazi + its preview/runtime tools, from `nixpkgs-yazi` so the
      # exact versions are frozen in flake.lock independently of the main nixpkgs.
      # superzej drives THIS yazi for the file-manager drawer (a private binary
      # via SUPERZEJ_YAZI_BIN + a private YAZI_CONFIG_HOME), never the system one.
      yaziPkgs = import nixpkgs-yazi {inherit system;};
      yaziPinned = yaziPkgs.yazi;
      # yazi's preview/runtime deps (fzf + zoxide are already in runtimeDeps).
      # `poppler-utils` (pdftoppm/pdftotext) is selected by attrpath — its hyphen
      # makes it unusable as a bare identifier inside `with`.
      yaziDeps =
        (with yaziPkgs; [
          file
          ffmpegthumbnailer
          unar
          jq
          fd
          ripgrep
          imagemagick
        ])
        ++ [yaziPkgs.poppler-utils];
      superzej = pkgs.callPackage ./nix/package.nix {
        zellij = zellijPinned;
        yazi = yaziPinned;
        inherit yaziDeps;
      };

      # One rust-overlay toolchain (clippy/rustfmt/rust-analyzer included) that
      # can also target wasm32-wasip1 — used for the dev shell and the plugins.
      rustToolchain = pkgs.rust-bin.stable.latest.default.override {
        targets = ["wasm32-wasip1"];
        # llvm-tools for `cargo llvm-cov` (just coverage).
        extensions = ["llvm-tools-preview"];
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
      # The pinned yazi superzej drives for the file-manager drawer.
      packages.yazi = yaziPinned;
      packages.superzej-sidebar = sidebar;
      packages.superzej-panel = panel;
      packages.superzej-tabbar = tabbar;
      packages.superzej-statusbar = statusbar;

      # `nix fmt` formats every tracked file via treefmt.toml.
      formatter = treefmtWrapper;

      checks = {
        # `nix flake check` gates on a clean build, formatting, clippy, and the
        # two wasm plugins building.
        build = superzej;
        plugin-sidebar = sidebar;
        plugin-panel = panel;
        plugin-tabbar = tabbar;
        plugin-statusbar = statusbar;
        formatting =
          pkgs.runCommand "treefmt-check" {
            buildInputs = fmtPackages ++ [pkgs.treefmt pkgs.git];
          } ''
            set -euo pipefail
            cp -r ${self} src
            chmod -R u+w src
            cd src
            treefmt --config-file=${self}/treefmt.toml \
              --no-cache --fail-on-change --tree-root .
            touch $out
          '';
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
            # task runner + formatter (treefmt wrapper with all formatters on PATH)
            just
            treefmtWrapper
            # line-coverage for `just coverage`
            cargo-llvm-cov
            # linters
            shellcheck
            yamllint
            taplo
            # pty visual-regression harnesses (test/*.py reconstruct the screen)
            (python3.withPackages (ps: with ps; [pyte]))
            # runtime tools superzej shells out to
            git
            fzf
            gum
            lazygit
            delta
            gh
          ]
          # The same pinned zellij + yazi as the package, so `just start` runs
          # the versions superzej ships — not whatever is on the system — and the
          # yazi preview tools resolve on PATH inside the drawer.
          ++ [zellijPinned yaziPinned]
          ++ yaziDeps;
        shellHook = ''
          export PATH="$PWD/target/debug:$PATH"
          # Point dev superzej at the pinned zellij + yazi (the package wires these too).
          export SUPERZEJ_ZELLIJ_BIN="${zellijPinned}/bin/zellij"
          export SUPERZEJ_YAZI_BIN="${yaziPinned}/bin/yazi"
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
