# THE-11 revision 1 completion

## Fixed

- Finding 1: the effective drawer registry now contains built-in files plus
  configured worktree and global tools. Palette, selection, cycling, toggle,
  and persistence write the declared scope; global selection clears only the
  active worktree destination, preserving the independent global choice across
  worktree switches.
- Finding 2: `DrawerRuntime` is the sole drawer lifecycle owner. Activation,
  tab/worktree switching, file reveal, prewarm, PTY completion, exit cleanup,
  geometry reconciliation, and pool operations all route through it. Runtime
  reconciliation runs after active-directory changes. Files prewarm results are
  tracked and pooled even before a desired open slot exists.
- Finding 3: the drawer OpenSpec proposal, design, delta spec, and tasks now
  describe the accepted `[[tools]]` metadata model, process-local global panes,
  no repo-local registry, deferred snapshots, strict validation, and the
  runtime lifecycle. Required scope, deduplication, indicator, and lifecycle
  scenarios are present; snapshot work remains explicitly deferred.

## Tests and verification

- `env XDG_RUNTIME_DIR=/tmp TMPDIR=/tmp RUSTC_WRAPPER= just quick thegn-core`
- `env XDG_RUNTIME_DIR=/tmp TMPDIR=/tmp RUSTC_WRAPPER= just quick thegn-host`
- `cargo nextest run -p thegn-host drawer_state::tests:: --no-fail-fast` — 9 passed
- `cargo nextest run -p thegn-host drawer_palette --no-fail-fast` — 1 passed
- `cargo nextest run -p thegn-core config_drawer::tests:: --no-fail-fast` — 7 passed
- `cargo clippy -p thegn-core -p thegn-host --tests -- -D warnings` — passed
- `nix develop --command treefmt` — passed, 0 files changed
- `nix run .#openspec -- validate --all --strict` — 170 passed, 0 failed

## Unverified

- End-to-end tests and snapshot rerenders were not run, per the revision
  dev-loop restriction and the accepted design's deferred snapshot scope.
- Full-workspace `just test`, `just ci`, coverage, migrations, and live-state
  binary runs were not performed.

## Disputed

None. All architect-review findings were addressed.
