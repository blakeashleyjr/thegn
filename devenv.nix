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
    fzf
    gum
    lazygit
    yazi
    delta
    gh
  ];

  # Use the nixpkgs toolchain (no channel/rust-overlay) so rustfmt/clippy match
  # the flake's treefmt + checks exactly — avoids formatter version skew.
  languages.rust.enable = true;

  # Pre-commit hooks: ONE formatter hook (treefmt) + linters only.
  # treefmt reads treefmt.toml at the repo root — single source of formatter
  # config shared with `nix fmt` (flake formatter).
  #
  # Run the suite on merges too, not just plain commits. A clean auto-merge of
  # two individually-valid branches can produce a semantically broken tree (e.g.
  # one branch changes a fn signature while another adds a now-stale call site —
  # different files, so no text conflict, so merge succeeds). `git merge` fires
  # `pre-merge-commit`, NOT `pre-commit`, so without this the merge result is
  # never linted. Listing pre-merge-commit makes the module install that hook.
  git-hooks.default_stages = ["pre-commit" "pre-merge-commit"];

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
    # coverage gate runs on pre-push (and in CI via `just ci`).
    cargo-test = {
      enable = true;
      name = "cargo test";
      entry = "cargo test --workspace";
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
    smoke = {
      enable = true;
      name = "smoke (hermetic CLI verbs)";
      entry = "just smoke";
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
    cargo build --workspace
    ./test/smoke.sh target/debug/szhost
  '';
}
