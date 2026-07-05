{pkgs, ...}: {
  # Developer environment for superzej. `devenv shell` to enter, `devenv test`
  # to run the git-hooks + smoke test.

  packages = with pkgs; [
    # task runner
    just
    # coverage gate (`just coverage`) + visual-regression harness
    cargo-llvm-cov
    python3
    # startup benchmarks (`just bench`)
    hyperfine
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
    # dependency gates (`just deps-audit`)
    cargo-deny
    cargo-machete
    # runtime tools superzej shells out to
    git
    fzf
    gum
    lazygit
    yazi
    delta
    gh
    # spec-driven development CLI (hermetic, pinned; see nix/openspec.nix).
    # superzej manages its own development with OpenSpec — `just openspec*`.
    nodejs
    (pkgs.callPackage ./nix/openspec.nix {})
  ];

  # OpenSpec: telemetry off by construction (matches the flake dev shell).
  env.OPENSPEC_TELEMETRY = "0";
  env.DO_NOT_TRACK = "1";

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
    #
    # git hooks run with GIT_DIR and GIT_INDEX_FILE set. This leaks into the
    # git subprocesses spawned by `cargo test`, causing spurious failures in
    # repository manipulation tests. Strip them via `env -u` so tests run in a
    # clean git environment. Likewise drop SUPERZEJ_SANDBOX: committing from a
    # shell running inside a live superzej bwrap sandbox leaks the =1 marker
    # into the runner and false-fails the sandbox argv tests.
    cargo-test = {
      enable = true;
      name = "cargo test";
      entry = "env -u GIT_DIR -u GIT_INDEX_FILE -u SUPERZEJ_SANDBOX cargo test --workspace";
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

    # Install the post-checkout hook into the effective (shared) hooks dir so the
    # prek hooks work in EVERY worktree. prek needs .pre-commit-config.yaml in
    # each worktree root, but devenv only materializes that gitignored store
    # symlink in the checkout where the shell is entered; the hook seeds it into
    # every other worktree on `git worktree add`. Copied (not symlinked) so it
    # doesn't depend on any one worktree's path, and refreshed on every entry so
    # it self-heals. See test/git-hooks/post-checkout.sh.
    hooks_dir=$(git config core.hooksPath 2>/dev/null || true)
    [ -n "$hooks_dir" ] || hooks_dir=$(git rev-parse --git-common-dir 2>/dev/null)/hooks
    if [ -d "$hooks_dir" ] && [ -f test/git-hooks/post-checkout.sh ]; then
      install -m 0755 test/git-hooks/post-checkout.sh "$hooks_dir/post-checkout"
    fi

    # Seed the gitignored Claude Code /opsx commands + skills if this checkout
    # lacks them (idempotent; cheap). See "Spec-driven development" in CLAUDE.md.
    if [ ! -d .claude/commands/opsx ] && [ -f openspec/config.yaml ]; then
      openspec init --tools claude --profile core --force >/dev/null 2>&1 || true
    fi
  '';

  # `devenv test` runs the hooks, then this.
  enterTest = ''
    cargo build --workspace
    ./test/smoke.sh target/debug/szhost
  '';
}
