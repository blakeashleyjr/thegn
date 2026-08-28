# Improve the agent pipeline v2 — the Lead's hand-rolled loop, made native

Linear: THE-76

## Why

The first live batch ran a multi-stage pipeline (architect → coders → review) with
a Lead agent executing the `[[pipeline.stages]]` org chart by hand, per worker:
render the stage prompt, seed permissions, open a session, write a roster row,
set it running — then poll a list, guess whether the worker really finished, and
mark it done. Every one of those steps has a failure mode the pilot actually
hit:

- a stage's prompt rendered **empty** (the Lead's own substitution ate itself on
  issue bodies full of braces) and a session was still opened;
- a worker wrote its handoff artifact but never committed it, and the Lead
  recorded `done` anyway — "session exit ≠ done";
- seeding `.claude/settings.local.json` **clobbered** the user's unrelated keys;
- an issue body containing `{issue_body}` got **re-parsed** by the Lead's
  hand-rolled substitution;
- the daemon resolved stage agents against a **stale boot config** after an
  `[[agents]]` rename;
- several stages sharing one worktree made the pane-exit → roster attribution
  ambiguous, and `dispatch set-status` accepted any claim with no verification.

Doctrine unchanged (`add-agent-orchestration-surface`,
`config_pipeline.rs` "structure, not judgment"): thegn never advances `next`,
never enforces `concurrency`, never fires `timeout_secs`. What it gains is the
ability to (a) _perform_ one dispatch atomically when asked, (b) _verify_ a
claim about a finished row, and (c) _block_ until something happens. Deciding
what to dispatch, whether a verified result is good, and what to do next stays
the Lead's.

## What Changes

1. **Core policy module** (`thegn_core::pipeline_run`, pure): sanitized
   per-issue artifact paths (`.thegn/pipeline/<ISSUE>/<stage>/<row>.md`), the
   run-completion verdict (only `done` is gated, only for rows carrying an
   artifact; untracked ⇒ refuse and say "commit it"; dirty is reported, never
   blocking), wait-target selection (`Spawning | Running` with a session —
   deliberately narrower than `is_active`), the pure
   `.claude/settings.local.json` allow-list merge (never overwrites a file it
   does not understand), and a narrow daemon registry refresh (`agents`,
   `tools`, `pipeline` only).
2. **`[[pipeline.stages] permissions`** — per-stage tool patterns seeded into
   the worktree's harness settings at dispatch, validated at
   `config validate` time, documented in `config/config.toml.example`.
3. **The roster field stamp** — `stamp_dispatch_run(id, session_id,
artifact_path)`, the roster's only field update: neither value is knowable
   until the row id exists and the session has opened. No schema change
   (columns are v56).
4. **CLI verbs** (chunks 2/3): `session open --stage` (atomic dispatch:
   row → session → stamp → running, failed row named on open failure),
   `dispatch verify`, `dispatch wait`, `session close`, `session list --live`,
   and the gated `set-status done`; two CLI-only catalog rows
   (`dispatches.verify` / `dispatches.wait`); daemon registry freshness at
   agent resolution.

## Impact

- **Roadmap**: group **Q** — Q 212 (task→worktree→agent→review→merge pipeline:
  the mechanism half), Q 215/224 (queue/batch: the roster + wait surface the
  Lead drives), Q 223 (task history: the durable roster). Linear **THE-76**.
- **Specs**: `agent` (ADDED: stage dispatch, permission seeding, the
  run-completion contract, the wake primitive, daemon registry freshness),
  `cli` (ADDED: session close / list --live / open --stage, dispatch verify /
  wait, the gated set-status done).
- **AI-free shell**: strictly additive. Every verb works with no agent
  configured; a row with no artifact is never gated; a pipeline with no stages
  changes nothing.
- **No DB schema change**: the roster's pipeline columns are v56;
  `SCHEMA_VERSION` is unchanged.
- **Chunks**: strictly serial (2 and 3 both edit `cmd/session.rs` and
  `test/smoke.sh`); see `tasks.md`.
