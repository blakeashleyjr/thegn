# Chunk 1 — THE-84: a resurrected tab relaunches its remembered agent (resume-aware) — DONE

Commit: `fceee179` — `feat(the-84): a resurrected tab relaunches its remembered agent (resume-aware)`
Branch: `tg/the-84-restart-blank-shell` (base `6b794610`, architect docs).

## What was implemented

Implements `openspec/specs/agent/spec.md` ("The worktree remembers its agent" →
"session resurrection relaunches the remembered agent"): a worktree tab whose
panes are re-resolved after a restart now relaunches the worktree's remembered
agent (`worktrees.agent`) instead of a blank login shell — resume-aware, with
the remembered record never rewritten.

### Files (exactly the chunk's list)

- `crates/thegn-host/src/handlers/worktree_launch.rs` — **NEW**. The relaunch
  decision + its unit tests:
  - `remembered_agent_relaunch(cfg, worktree, leaf) -> Option<(u32, LaunchSpec)>`
    — the spec's decision ladder, every gate fail-open to `None` (= today's
    shell): remembered row (missing/`shell`/`clean-shell`/tool drawer ⇒ None,
    same exclusions as the native-exec path `panes.rs:862-868`) → entry still
    configured (`agent_command`, stale record left alone) → resume form resolved
    FIRST (cheap `entry.resume` read; the bounded session walk never runs for
    non-opted entries) → spec composed via
    `direnv_warm::launch_spec_synced_with` with `suppress_agent_record: true`
    always (full sandbox/credential/cap parity by construction; second direnv
    warm is a cached no-op). Resume chain: `sessions::discover` (bounded,
    worktree-filtered, newest-first) → `agent_task::auto_resume_id` (re-checks
    opt-in, RESUME cap, id shape) → `daemon::agent_open::command_for` resume
    form; any miss ⇒ cold launch.
  - `apply_relaunch` — the call-site fold shared by all three workers (see
    "Deviation" below).
- `crates/thegn-host/src/handlers/mod.rs` — one `pub(crate) mod worktree_launch;`
  line (alphabetical, between `worktree_delete` and `worktree_rename`).
- `crates/thegn-host/src/handlers/materialize.rs` — call-site-only edits: after
  the THE-85 attach probe, BOTH shell branches (warm-spare + post-provision)
  fold the override in; gates exactly as prescribed (`attach.is_empty()`,
  `!quiet`), first missing leaf in tree order.
- `crates/thegn-host/src/run.rs` — the chunk's ONLY run.rs hunk: the prewarm
  worker captures `first_leaf` before its resolve moves `missing`, then (after
  the attach probe) folds the override gated on `!is_terminal` (terminal groups
  host no agent sessions) and `attach.is_empty()`; a prewarm is never a split,
  so `quiet_split: false`.
- `crates/thegn-host/src/daemon/agent_open.rs` — `command_for` →
  `pub(crate)` (visibility only; body untouched; doc line added).

### Properties preserved

- Live session wins: the fold is gated on the THE-85 probe being empty — the
  agent is never doubled, and the relaunched session is worktree-tagged
  end-to-end so the NEXT open attaches via THE-85 (no second dedup mechanism).
- `worktrees.agent` is never written by a relaunch (`suppress_agent_record`
  always) — unit-tested byte-identical before/after.
- No config keys, no help-page/action ids (help ratchet untouched), no spec
  deltas, no color/glyph literals, no platform `#[cfg]`, no new ignored
  `Result`s (DB reads are Option-chained; the walker never errors; fail-open
  `.ok()`s carry their "why" comments).
- Agent argv lands on the FIRST missing leaf; remaining leaves keep the
  resolved shell argv (unit-tested).

## Deviation (deliberate, for the prescribed worker-level test)

The chunk's "Call-site shape" shows the gates + let-chain inline at each site.
I extracted that exact shape into `worktree_launch::apply_relaunch` (same
gates, same let-chain, same first-leaf semantics) so the chunk's required
worker-level test ("first leaf carries the agent argv, remaining leaves the
shell argv; non-empty attach / quiet split ⇒ batch untouched") exercises the
real fold rather than a test-side replica. Call sites remain call-site-only
edits (materialize: one `let mut` + one call; run.rs: one capture line + one
guarded call).

## Gates run (per the dev-loop policy — no full-workspace compiles)

- `just quick thegn-host` — clean (clippy, lib/bin only).
- `cargo nextest run -p thegn-host worktree_launch` — **8/8 pass** (all new
  tests, incl. the 7 listed in the chunk + the worker-level batch test).
- `cargo nextest run -p thegn-host materialize` — 8/8 pass (incl. THE-85's
  suppression test).
- `cargo nextest run -p thegn-host agent_open` — 12/12 pass (visibility bump
  inert, incl. the resume-refusal parity tests).
- `cargo nextest run -p thegn-host prewarm` — 4/4 pass (run.rs-adjacent).
- The scoped tests were re-run against the committed (treefmt-formatted)
  content: 8/8 pass.

Test isolation: every new test serializes on `crate::testenv::ENV_LOCK` via
`EnvVarGuard` and redirects `XDG_STATE_HOME` to a scratch dir (the shell often
runs inside a live thegn); session discovery is pointed at a seeded
`[[usage.accounts]]` home (explicit-beats-implicit, so the real `~/.claude`
never leaks in).

## Unverified

- **No live/e2e run.** Per the lead's instructions e2e was not run and no
  heavy gates (`just test`, `just ci`, `just coverage`) were executed. The
  end-to-end behavior (open a worktree tab after a daemon restart → agent
  spawns; resumed when opted-in + RESUME-capable + a session exists; attach
  wins when a live session exists) is covered by the unit tests at the
  decision/fold level and by the unchanged THE-85 machinery, but was not
  exercised against a running daemon — left to the review/e2e stage.
- `just quick thegn-host` (clippy) ran on the pre-format content; the only
  post-clippy change is the pre-commit `treefmt` pass (rustfmt whitespace,
  canonical per the hook), and the committed content compiles + passes the
  scoped nextest filters above.
- Machine-dependent wall-clock cost of the extra decision path (one `Db::open`
  - a compose per bring-up of an agent-bearing worktree) is not measured; it
    is the same order as the shell resolve it replaces and stays on the workers'
    `spawn_blocking` threads (0%-idle contract untouched).
- Coverage gate (`thegn-core` 95%) unaffected — no core changes.
