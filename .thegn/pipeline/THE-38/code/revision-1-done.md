# THE-38 revision 1 completion

## Resolved findings

- Finding 1: restored repo metrics command-collector refusal visibility in
  `ConfigHealth`. The selected candidate body and format are passed to a core
  helper, so validation and doctor health report a path-prefixed warning naming
  the target without a second discovery/read path or command execution.
- Finding 2: repo discovery now retains existing candidates whose contents
  cannot be read, including their path and read error but never their contents.
  Readable selection remains tolerant and unchanged; health reports every
  unreadable candidate as a path-owned validation problem.

## Commits

- `65f85a03 fix(the-38): restore repo metrics refusal visibility (revision 1)`
- `1ed3ae16 fix(the-38): retain unreadable repo candidates (revision 1)`

## Verification

- `cargo nextest run -p thegn-core config_repo`: 8 passed.
- `cargo nextest run -p thegn-host config_health`: 4 passed.
- `just quick thegn-core`: passed.
- `just quick thegn-host`: passed.
- `cargo clippy -p thegn-core --tests -- -D warnings`: passed.
- `cargo clippy -p thegn-host --tests -- -D warnings`: passed.
- `treefmt` in the repository dev shell: passed, 0 files changed.

All compilation checks used `RUSTC_WRAPPER=` and temporary runtime/cache paths
because the default sccache, runtime, and cache locations are read-only in this
worktree environment.

## Unverified

Per the dev-loop policy, full-workspace `just test`, `just ci`, coverage, and
e2e were not run. The repository also has no `test/ratchet-check.sh`.

## Disputed

None.
