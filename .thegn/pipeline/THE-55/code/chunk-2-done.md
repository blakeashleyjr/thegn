# THE-55 chunk 2 completion

Implemented the host-side `thegn session move` operation and user-facing
documentation on top of chunk 1.

- Added `session move <worktree> --to-profile <name> [--kill] [--dry-run]
[--json]` and dispatched it before the ordinary daemon connection, so cold
  migrations work without a source daemon.
- Added target-first orchestration over the source and target SQLite stores:
  exact worktree/session selection, target conflict/resume planning,
  source-daemon liveness checks, explicit kill/re-list confirmation, target
  import/read-back confirmation, exact source cleanup, and retryable partial
  results.
- Added redacted human/JSON audit output with profiles, exact worktree,
  groups, per-table counts, liveness/kill IDs, target/source state, resume
  state, target dispatch IDs, and best-effort target notification status.
  Opaque pane commands, scrollback, reports, notes, and credentials are never
  emitted.
- Added target daemon notification discovery from the target DB registry
  without rerooting, loading target config, or applying target credentials.
- Documented syntax, row scope, cold/live/kill/dry-run behavior, target-first
  resume semantics, daemon-ID remapping, and the credential boundary in both
  CLI help pages.

## Verification

- `XDG_RUNTIME_DIR=/tmp TMPDIR=/tmp RUSTC_WRAPPER= CARGO_TARGET_DIR=/tmp/tg-the-55-target just quick thegn-host`
- `cargo nextest run -p thegn-host session_move` — 2 passed
- `cargo nextest run -p thegn-host completion_slots_are_bound_or_pinned` — passed
- `cargo nextest run -p thegn-host action_docs_ratchet` — passed
- `cargo nextest run -p thegn-svc control_wire_matches_the_committed_snapshot` — passed
- Isolated CLI help check for `session move`, `--to-profile`, `--kill`,
  `--dry-run`, and `--json` — passed

## Unverified

- Full workspace gates (`just test`, `just lint`, `just ci`, coverage, smoke,
  and e2e) were not run per the chunk/dev-loop policy.
- The first normal-target quick/test attempts were blocked by the shared Cargo
  build lock and `just` runtime temp path; verification was rerun with
  `XDG_RUNTIME_DIR`, `TMPDIR`, and `CARGO_TARGET_DIR` isolated under `/tmp`.
