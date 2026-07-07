{pkgs, ...}: {
  # Developer environment for superzej. `devenv shell` to enter, `devenv test`
  # to run the git-hooks + smoke test.

  packages = with pkgs; [
    # task runner
    just
    # faster test runner (`just test` → `cargo nextest run`) + faster linker
    # (mold, wired via CARGO_TARGET_*_RUSTFLAGS below). Both cut the pre-push
    # + CI compile/test cost.
    cargo-nextest
    mold
    # compilation cache (RUSTC_WRAPPER below). superzej is a many-worktree
    # workflow and each `git worktree` has its own cold target/; sccache shares
    # compiled crate artifacts across all of them (and across branch switches),
    # so a fresh worktree's first build is warm instead of from-scratch.
    sccache
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

  # Link with mold on the linux-gnu host triple — a large cut to incremental
  # link time for every cargo invocation (build/clippy/test/coverage). Scoped
  # to THIS triple via the CARGO_TARGET_<triple>_RUSTFLAGS env var (not
  # .cargo/config.toml) so it never touches the `check-cross` macOS/Windows/wasm
  # targets and never leaks into the `nix build .#default` package derivation
  # (which doesn't enter this shell — so it needs no mold build input). Requires
  # mold on PATH (added to `packages` above); gcc's `-fuse-ld=mold` picks it up.
  env.CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS = "-C link-arg=-fuse-ld=mold";

  # Compilation cache. sccache caches per-crate rustc invocations so cold
  # worktrees / branch switches reuse artifacts instead of recompiling from
  # scratch. sccache and Cargo incremental compilation are mutually exclusive
  # (incremental bypasses the cache), so CARGO_INCREMENTAL=0 lets sccache work;
  # the fast single-crate iterative path is `just quick <crate>`. Dev-shell only
  # — the `nix build .#default` package derivation never enters this shell.
  env.RUSTC_WRAPPER = "sccache";
  env.CARGO_INCREMENTAL = "0";

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
    # linters — these are checks, not formatters; kept as separate hooks.
    # shellcheck/yamllint are cheap + staged-file, so they stay on pre-commit.
    # clippy compiles the whole workspace, so it moves to pre-push (see below).
    clippy = {
      enable = true;
      stages = ["pre-push"];
    };
    shellcheck.enable = true;
    yamllint.enable = true;

    # god-file ratchet: legacy oversized files may only shrink, new files are
    # hard-capped at 3000 lines (test/file-size-ratchet.sh, also in `just lint`).
    # ~1s and reads the tree, not staged files — cheap enough to catch growth at
    # commit time rather than only in CI.
    file-size-ratchet = {
      enable = true;
      name = "god-file ratchet";
      entry = "bash test/file-size-ratchet.sh";
      language = "system";
      pass_filenames = false;
      stages = ["pre-commit"];
    };

    # ── Tiered gates ──────────────────────────────────────────────────────
    # pre-commit stays CHEAP (formatting + shell/yaml lint + the god-file
    # ratchet) so commits are near-instant. The correctness gates — clippy, the
    # full test suite, and smoke — run on pre-push (before code leaves the
    # machine) and in CI via `just ci`. This defers the semantic-merge check
    # (a stale call site across a clean auto-merge) from merge time to push time;
    # it is still caught before the merge is pushed, and always by CI.
    #
    # Coverage (`cargo llvm-cov`) is NOT on pre-push: it is an instrumented full
    # recompile into a separate target dir (the single heaviest gate) and CI
    # re-runs it anyway. It stays a CI-only gate via `just ci`. Run it locally
    # on demand with `just coverage` before opening a PR.
    #
    # git hooks run with GIT_DIR and GIT_INDEX_FILE set. This leaks into the
    # git subprocesses spawned by `cargo test`, causing spurious failures in
    # repository manipulation tests. Strip them via `env -u` so tests run in a
    # clean git environment. Likewise drop SUPERZEJ_SANDBOX: committing from a
    # shell running inside a live superzej bwrap sandbox leaks the =1 marker
    # into the runner and false-fails the sandbox argv tests. `just test` runs
    # cargo-nextest (faster) + a doctest pass — one source of truth with CI.
    cargo-test = {
      enable = true;
      name = "cargo test";
      entry = "env -u GIT_DIR -u GIT_INDEX_FILE -u SUPERZEJ_SANDBOX just test";
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

    # Leave headroom so heavy builds don't peg the machine. CARGO_BUILD_JOBS
    # caps the parallel rustc/codegen jobs cargo spawns; computed at shell entry
    # since Nix eval can't see the core count. Respect an already-set value.
    if [ -z "''${CARGO_BUILD_JOBS:-}" ]; then
      _jobs=$(nproc 2>/dev/null || echo 4)
      if [ "$_jobs" -gt 2 ]; then export CARGO_BUILD_JOBS=$((_jobs - 2)); else export CARGO_BUILD_JOBS=1; fi
    fi

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
