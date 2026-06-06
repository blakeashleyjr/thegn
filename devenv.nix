{pkgs, ...}: {
  # Developer environment for superzej. `devenv shell` to enter, `devenv test`
  # to run the git-hooks + smoke test.

  packages = with pkgs; [
    # task runner
    just
    # coverage gate (`just coverage`) + visual-regression harness
    cargo-llvm-cov
    python3
    # treefmt — reads treefmt.toml; versions must match the flake's nixpkgs
    treefmt
    # formatter binaries treefmt.toml references (rustfmt comes from languages.rust below)
    alejandra
    shfmt
    taplo
    yamlfmt
    prettier
    # linters (not formatters — kept as separate pre-commit hooks)
    shellcheck
    yamllint
    # runtime tools superzej shells out to
    git
    zellij
    fzf
    gum
    lazygit
    yazi
    delta
    gh
  ];

  # The WASM plugins (plugin/sidebar, plugin/panel) build via the flake
  # (`just nix-build-plugins`) or install.sh, which provide the wasm32-wasip1
  # toolchain — kept out of this nixpkgs dev shell to avoid formatter skew.

  # Use the nixpkgs toolchain (no channel/rust-overlay) so rustfmt/clippy match
  # the flake's treefmt + checks exactly — avoids formatter version skew.
  languages.rust.enable = true;

  # Pre-commit hooks: ONE formatter hook (treefmt) + linters only.
  # treefmt reads treefmt.toml at the repo root — single source of formatter
  # config shared with `nix fmt` (flake formatter).
  git-hooks.hooks = {
    # formatting — delegate ALL formatters to treefmt via treefmt.toml
    treefmt = {
      enable = true;
      settings.formatters = with pkgs; [
        # rustfmt is provided by languages.rust above; list it here so the
        # hook wrapper puts it on PATH explicitly too.
        rustfmt
        alejandra
        shfmt
        taplo
        yamlfmt
        prettier
      ];
    };
    # linters — these are checks, not formatters; kept as separate hooks
    clippy.enable = true;
    shellcheck.enable = true;
    yamllint.enable = true;

    # ── Tiered gates ──────────────────────────────────────────────────────
    # pre-commit stays fast (formatting + lint + unit tests); the heavier
    # coverage / e2e / visual gates run on pre-push (and in CI via `just ci`).
    cargo-test = {
      enable = true;
      name = "cargo test";
      entry = "cargo test --bin superzej";
      language = "system";
      pass_filenames = false;
      stages = ["pre-commit"];
    };
    coverage = {
      enable = true;
      name = "coverage 95% (core)";
      entry = "just coverage";
      language = "system";
      pass_filenames = false;
      stages = ["pre-push"];
    };
    e2e = {
      enable = true;
      name = "e2e (sandboxed zellij)";
      entry = "just e2e";
      language = "system";
      pass_filenames = false;
      stages = ["pre-push"];
    };
    visual-regression = {
      enable = true;
      name = "visual regression";
      entry = "just visual";
      language = "system";
      pass_filenames = false;
      stages = ["pre-push"];
    };
  };

  enterShell = ''
    echo "superzej devenv — cargo build | just smoke | nix fmt"
  '';

  # `devenv test` runs the hooks, then this.
  enterTest = ''
    cargo build
    ./test/smoke.sh target/debug/superzej
  '';
}
