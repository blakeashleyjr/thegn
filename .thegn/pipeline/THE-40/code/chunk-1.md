# THE-40 chunk 1 — publish the native GUI decision record

## Scope

Publish the decision record as documentation only: native GUI is not built in
THE-40; a future candidate-2 GPU cell client is a separate thin client of the
daemon/control API; web access is a separate remote-access lane; native chrome
waits for a serializable view model. Sync the existing openspec draft to that
decision and archive it as a completed decision record.

Do not add code, dependencies, GUI toolkit crates, routes, catalog rows,
config keys, DB changes, migrations, or ratchet entries. In particular, do not
implement the draft’s proposed `deny.toml`/crate-boundary changes: they are
implementation policy for a future frontend crate, not part of this no-code
decision.

## Exact files touched

Create/update the following final documentation paths:

- `docs/superpowers/specs/2026-08-29-native-gui-frontend-lane-design.md`
- `openspec/changes/archive/2026-08-29-define-gui-frontend-lane/proposal.md`
- `openspec/changes/archive/2026-08-29-define-gui-frontend-lane/design.md`
- `openspec/changes/archive/2026-08-29-define-gui-frontend-lane/tasks.md`
- `openspec/changes/archive/2026-08-29-define-gui-frontend-lane/specs/architecture-gates/spec.md`

The active source paths being synchronized/archived are the corresponding
existing files:

- `openspec/changes/define-gui-frontend-lane/proposal.md`
- `openspec/changes/define-gui-frontend-lane/design.md`
- `openspec/changes/define-gui-frontend-lane/tasks.md`
- `openspec/changes/define-gui-frontend-lane/specs/architecture-gates/spec.md`

Treat active-to-archive as a rename: the final tree must not retain duplicate
active and archived copies. Do not touch `CLAUDE.md`, `docs/ARCHITECTURE.md`,
`tasks.md`, `deny.toml`, Rust sources, `config/config.toml.example`,
`docs/api/control-v1.json`, or any test/ratchet file.

## Approach

1. Copy the architect decision into the dated superpowers spec. Keep the
   evidence table and correct the draft’s stale claims: attach/multi-subscriber
   behavior, pairing page/CORS, the incomplete binary-frame schema, and the
   absence of a server-side layout/chrome model.
2. Rewrite the openspec proposal/design/tasks/spec so the change is explicitly
   a decision record with no implementation. Preserve the three requested
   candidate shapes and their trade-offs, the one-catalog/0%-idle/shell-
   independence invariants, THE-34 coordination, and the THE-40-F1 observer
   cell-client follow-up.
3. Remove or mark complete any draft task that would edit `deny.toml`,
   `crates/thegn-core/tests/crate_boundaries.rs`, `docs/ARCHITECTURE.md`, or
   `tasks.md`. The final openspec record must say those are future or separate
   implementation work, not claim they landed here.
4. Archive the synchronized change with the repository’s normal OpenSpec
   archive layout. Do not run an archive command that rewrites unrelated
   changes; verify the resulting rename paths and that no active duplicate
   remains.

## Overlap and dependencies

This is the only THE-40 coder chunk. It is independent of all code work and
must run serially with any future frontend implementation because the decision
record is the source of architectural constraints. It has no file overlap with
THE-34’s implementation chunks, but it depends semantically on THE-34’s
documented filter/lag vocabulary; quote/reference that branch’s final contract
without designing a competing `events.subscribe` protocol.

## Tests to run

Documentation validation:

- `just openspec-validate`
- `git diff --check`

Required scoped architecture smoke checks (no full-workspace gate):

- `just quick thegn-core`
- `cargo nextest run -p thegn-core capability`
- `just quick thegn-host`
- `cargo nextest run -p thegn-host help`

These checks are expected to be unchanged by the docs-only patch; they verify
that the catalog/help/boundary assumptions cited by the record remain green.
Do not run `just test`, `just ci`, e2e, or a live daemon. If a manual `thegn`
invocation is necessary, set `XDG_STATE_HOME` to a fresh temporary directory
first and never use the worktree’s live state DB.

## Done criteria

- `docs/superpowers/specs/2026-08-29-native-gui-frontend-lane-design.md`
  records the not-now decision, substrate gap matrix, three candidate shapes,
  recommendation, invariants, and THE-40-F1 follow-up.
- The archived openspec proposal/design/tasks/spec agree with that document,
  contain no stale “private attach”, “single client”, or “/pair 404/no CORS”
  claims, and do not represent code/dependency/roadmap edits as landed.
- The final tree contains the archived change and no active
  `openspec/changes/define-gui-frontend-lane/` duplicate.
- No Rust, config, catalog, API schema, database, render path, dependency, or
  ratchet file changed.
- `just openspec-validate`, the scoped quick/nextest checks, and `git diff
--check` pass.
- Commit exactly as: `docs(the-40): publish native GUI decision record`
