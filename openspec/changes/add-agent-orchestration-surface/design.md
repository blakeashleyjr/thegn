# Design — agent orchestration surface

## Architecture: agent head, thegn hands

```text
  supervisor agent (one pane, or headless)
        │  MCP / CLI / HTTP — every door projects capability::CATALOG
        ├── issues.list/get ──────► IssueRouter (Linear GraphQL ships today)
        ├── worktrees.create ─────► worktree::add_checked + db.link_issue
        ├── dispatches.put ───────► agent_dispatches roster (schema v12)
        ├── sessions.open{agent} ─► agent_open::resolve → agent::launch_spec_full
        ├── sessions.wait ────────► per-session FSM + tombstones   (landed, d51ab92e)
        ├── sessions.snapshot ────► live screen, or the corpse's final screen
        └── issues.update/comment ► tracker transition + PR/branch link
                                    │
  worker agent per issue, in its own worktree ◄┘
```

Concurrency, attempt budgets, and the definition of "done" are prompt content
in the supervisor skill — deliberately not config, not code. The accepted
trade-offs of an agent supervisor (no native watchdog, no native resumability)
are mitigated as _tools_: `sessions.wait` takes `timeout_ms` (the skill
mandates always passing one), and the `agent_dispatches` table is durable
roster state a restarted supervisor reads back.

## Retroactive deltas (substrate landed unrecorded)

Commit `d51ab92e` landed launch-by-name, the activity FSM, `wait --until`, and
tombstones with no change folder. The `agent` and `control-plane` deltas in
this change record that behaviour as it exists; implementation tasks for those
requirements are test-and-document only (the daemon session/service modules
carry ~870 new lines with thin coverage — the wait/tombstone ordering tests are
part of this change).

## The capability-row ratchet

Each new row is the fixed six-step: `Verb` variant + `Verb::ALL`, a
`required_scope` arm, a `cap(...)` row in `CATALOG`, a `ControlApi` method, an
HTTP route naming the id, then implement per surface or record a
`SURFACE_GAPS` entry with a reason (`routes_cover_catalog` and
`every_verb_has_exactly_one_row` enforce it — never a second policy table).
Scopes: `issues.list/get` and `dispatches.list` Read; `issues.update/comment`
and `dispatches.put/set_status` Write; `worktrees.create` Git. gRPC: mirror in
`control.proto` or record the gap explicitly — the existing
`sessions.wait`/`sessions.split` gRPC gaps set the precedent.

`worktrees.create` reuses the branch-derivation rule the `D` key already
implements (tracker `branch_hint`, else the naming fallback) so the TUI action
and the headless door cannot drift; `add-issue-driven-worktrees`'s `s` key sits
on the same pipeline.

## Dispatch-status fix

`AgentDispatchStatus` gains `Done` and `Failed` plus a `parse(&str)` that also
accepts the legacy lowercase strings already written to disk (`"done"`,
`"failed"`, and the existing seven). Both writers — `pty_drain.rs` (pane exit)
and the tracker handler — construct the enum and store `as_str()`. Reads
coerce unknown strings to a visible `Unknown`-style presentation rather than
erroring, per the never-reset-user-data contract. Pure enum logic lands in
`thegn-core` with round-trip tests (95% gate); no DDL change.

## CLI

`SessionAction::Open` mirrors `AgentLaunch` (`--agent`, `--prompt`,
`--worktree`, `--headless`, `--bind`, `--json`), following `cmd/merge.rs`
shape, `cmd::target::WorktreeTarget`, and the one-emitter `--json` convention.
`thegn dispatch` is a new noun namespace (list/set-status). `thegn issue list`
gains `--status`/`--limit`. All list-shaped reads follow the documented
exit-code + JSON contract in the `cli` spec.

## The supervisor skill

A skill under `extensions/skills/` (dev tooling, not shipped binary code)
encoding the loop and its judgement calls: take N issues, always pass
`timeout_ms`, on `blocked` snapshot and either answer via `sessions.input` or
park as `waiting_human`, on `done` take the run's configured exit, resume from
`dispatches.list`, never re-dispatch a `Running` row. Fan-out (one task → N
workers, the archived `add-agent-team-fanout` axis) is expressible with the
same primitives — the skill documents the pattern; no native verb exists.

## Event-loop / render notes

No new loop work: every operation is request-driven through the control plane
(daemon side), and the existing event feed + `TerminalWaker` path carries
session-state edges. No render-plan change; anything visible (roster in the
panel, notifications) rides existing damage channels.

## Alternatives considered

- **A native `fleet drain` driver** — explicitly rejected by the 2026-08-24
  decision; every driver feature (watchdog, budget, concurrency) hard-codes
  judgement the prompt should own.
- **Reviving `add-fleet-view` as the orchestration surface** — it is
  observability, not orchestration, and its metrics source (LLM proxy) is
  excised. Flagged for archive/rework instead.
- **A `thegn team` fan-out verb** (archived `add-agent-team-fanout`) — the
  orthogonal axis (one task → N agents); reachable through the same primitives
  from a prompt, so no verb is added now.

## Security

- **New Write doors into the tracker** (`issues.update`, `issues.comment`)
  cross a trust boundary into an external system on the user's credentials.
  They are Write-scoped catalog rows: MCP exposure requires the (config-
  clamped) write scope, tokens stay in the existing tracker credential
  resolution (SecretRef/env:/file: — never raw in config), and every mutation
  is attributable in the tracker as the authenticated user.
- **Prompt injection**: issue titles/bodies are untrusted text rendered into
  worker prompts. They pass through the agent-task engine's shell-quoting
  contract (never string-spliced into `sh -lc`), and the rendered prompt is
  data to the worker agent — the skill instructs supervisors to treat issue
  content as task description, not as instructions to the supervisor itself.
- **`worktrees.create`** writes to the filesystem and git; it is Git-scoped,
  confined to the configured worktrees dir via the existing `add_checked`
  pipeline, and rolls back the git worktree if registration fails (existing
  `wt new` contract).
- **Worker blast radius**: workers launch through `agent::launch_spec_full` —
  the same sandbox, credential-carry, and `[sandbox.limits]` slice as
  interactive panes; a fleet cannot escape the resource ceiling interactive
  agents live under.
- **Roster integrity**: `dispatches.set_status` is Write-scoped; the roster is
  local SQLite (cache, not source of truth) and never feeds back into
  automatic actions — a corrupted status misleads a supervisor's read, it does
  not trigger thegn-side writes.

## Open questions

- Should `issues.update` expose the full `IssuePatch` (status + assignee) or
  status-only in the first cut? Leaning full patch — the router already has it.
- Whether `dispatches.put` should auto-transition `Queued → Spawning` when the
  same caller immediately opens a session, or leave every transition explicit
  (leaning explicit — the roster is the supervisor's ledger, not a state
  machine).
- gRPC mirroring for the new rows vs. recorded gaps — decide once the proto
  cost is visible; the catalog test passes either way.
