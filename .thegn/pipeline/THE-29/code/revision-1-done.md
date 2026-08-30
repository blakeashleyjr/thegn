# THE-29 revision 1 — done

## Findings fixed

- Forks capture the source session's live rows/columns, and scrollback handoff
  uses the shared 2,000-line snapshot bound.
- An explicit recorded harness remains authoritative. Configured agents whose
  provider does not match the source harness are rejected; matching agents
  retain the selected harness command while receiving fresh launch context.
- Fork orchestration and the common daemon PTY/session registration path were
  extracted into `daemon/fork.rs`; `DaemonService::fork` is now a thin boundary
  and `open` reuses the same spawn helper.
- The active OpenSpec proposal, design, control-plane delta, and task checklist
  now describe native recorded sources, final wire fields, catalog/MCP/plugin
  projection, and completed work. Only the full `just ci` task remains open.
- Added daemon-level hermetic coverage for the real `ControlApi::fork` path:
  source liveness, new id/pid, resized geometry, lineage environment,
  bounded owner-only handoff and cleanup, adopt placement, validation refusal,
  and dead-session refusal.

## Commits

- `634bc52a` — `fix(the-29): inherit source geometry and harness authority (revision 1)`
- `6b73e6c4` — `fix(the-29): cover daemon fork lifecycle (revision 1)`
- `e4311020` — `fix(the-29): synchronize OpenSpec fork surfaces (revision 1)`

## Verification

- `XDG_RUNTIME_DIR=/tmp RUSTC_WRAPPER= just quick thegn-host` — passed.
- `XDG_RUNTIME_DIR=/tmp RUSTC_WRAPPER= cargo clippy -p thegn-host --tests -- -D warnings` — passed.
- `treefmt --no-cache --allow-missing-formatter` — passed with no changes.
- Targeted `cargo nextest run -p thegn-host` fork tests — 5 passed, including
  `fork_control_path_inherits_geometry_and_cleans_handoff`.
- Pre-commit treefmt/shellcheck/yamllint hooks — passed on each commit.
- `git diff --check` — passed.

## Unverified

- `just openspec-validate` could not run because `openspec` is not on PATH.
  The bounded pinned-Nix attempt also failed before validation because the Nix
  fetcher cache is read-only. Strict OpenSpec validation therefore remains
  unverified.
- Full-workspace gates (`just test`, `just ci`, coverage, rustdoc, and e2e) were
  not run per the revision dev-loop restriction. The existing real-socket test
  remains unavailable in this environment because socket setup returns
  `Operation not permitted`; the new daemon integration test uses the direct
  `ControlApi` seam and passes without opening a socket.
- No live `thegn` invocation, migration, or normal XDG state database was used.
