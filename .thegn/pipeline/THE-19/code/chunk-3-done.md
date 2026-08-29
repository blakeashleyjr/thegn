# THE-19 chunk 3 completion

Documented the lifecycle-hook surface in the existing authored help pages.

- Added `[hooks]` guidance for global, `[workspace.<slug>]`, and trusted repo
  overlays, all six events, string/object entries, defaults, ordering, cwd,
  curated environment, timeout, `wait`, failure/force/unattended behavior,
  logs, notifications, and session latches.
- Documented the legacy `[sandbox].prepare` compatibility alias and the
  distinction between host lifecycle hooks and per-pane
  `[sandbox].init_script`.
- Documented lifecycle behavior across wizard/CLI/daemon/UI/merge paths and
  clarified that pipeline stages are structure-only; supervisors use `wt new`
  or `worktrees.create` for the shared lifecycle seam.
- Added a narrow smoke fixture/check for successful setup and a forced,
  failure-visible teardown. No completion-slot, help-ratchet, or control-schema
  snapshot changes were needed.

## Verification

- `env TMPDIR=/tmp XDG_RUNTIME_DIR=/tmp/thegn-runtime-the19 XDG_STATE_HOME=/tmp/thegn-state-the19 RUSTC_WRAPPER= just quick thegn-host` — passed.
- `cargo nextest run -p thegn-host help` — 75 passed.
- `env TMPDIR=/tmp XDG_RUNTIME_DIR=/tmp/thegn-runtime-the19 XDG_STATE_HOME=/tmp/thegn-state-the19 RUSTC_WRAPPER= just quick thegn-svc` — passed.
- `cargo nextest run -p thegn-svc control_wire_matches_the_committed_snapshot` — 1 passed; snapshot unchanged.
- `cargo nextest run -p thegn-core --test config_example` — 2 passed.
- `bash -n test/smoke.sh` and `git diff --check` — passed.

## Unverified

- The prescribed `cargo nextest run -p thegn-core config_example` filter selected no tests; the equivalent integration-test target above passed.
- `test/smoke.sh` was not executed because the chunk explicitly forbids running it as e2e here.
- Full-workspace gates (`just test`, `just ci`, coverage, and e2e) were not run per the chunk and dev-loop policy.
