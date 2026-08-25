# Add the agent orchestration surface (supervisor-agent tool set)

Linear: THE-57

## Why

The decided direction (2026-08-24) for subagent fan-out and conductor/leader
orchestration is: **the supervisor is an agent, not a Rust driver**. thegn does
not grow a drain loop, a scheduler, or a `fleet` verb — it grows the _hands_: a
tool surface complete enough that a Claude (or any) agent can be the head, plus
a skill that teaches the loop. Concurrency and what "done" means are prompt
content, chosen per run.

The supervision substrate already landed on main (commit `d51ab92e`): the
daemon launches a _configured_ agent by name through the same
sandbox/credential composition as the TUI (`sessions.open` + `AgentLaunch`,
`daemon/agent_open.rs`), runs a per-session activity FSM
(`blocked · working · done · idle`, edge-triggered on the event feed), supports
`sessions.wait --until idle|blocked|done|match:<rx>` with timeouts, and leaves
tombstones so a late `wait`/`snapshot` still gets the exit code and final
screen. **It landed with no openspec change folder** — undocumented design debt
this change repays with retroactive deltas.

What is missing is everything that holds a _list_ of work:

- No `issues.*` / `dispatches.*` / `worktrees.create` capability rows — a
  supervisor cannot enumerate a board, create a worktree, or read/advance the
  durable roster through the catalog surfaces.
- No CLI door onto the launch primitive: `thegn session` has
  List/Send/Snapshot/Attach/Wait/Split but no `open`, so `AgentLaunch` is
  HTTP-only today.
- **A live bug**: `pty_drain.rs:789` writes dispatch statuses `"done"` /
  `"failed"` that `AgentDispatchStatus` (Queued/Spawning/Running/WaitingHuman/
  PrOpen/Merged/Abandoned) cannot parse — the roster a supervisor would resume
  from is unparseable exactly at the rows that matter.
- The single-issue dispatch (`handlers/tracker.rs::dispatch_agent`, the `D`
  key) hardcodes `"claude"` at three sites and seeds only `THEGN_ISSUE_*` env —
  no rendered prompt.
- Nothing teaches the loop: the supervisor skill does not exist.

Separately, `openspec/changes/add-fleet-view` claims the `thegn fleet` verb for
a read-only metrics view whose design **depends on the excised LLM proxy**
(authoritative token metrics via `ProxyRequestRow`). It should be archived or
reworked; this change deliberately claims no `fleet` verb and nothing here
builds on it.

## What Changes

1. **Retroactive spec deltas** for the landed supervision substrate: agent
   launch by name over the control plane, the per-session activity state +
   conditional waits, and tombstones (deltas on `agent` and `control-plane`).
2. **Orchestration capability rows**, each the fixed catalog ratchet (Verb +
   `required_scope` + `cap(...)` + `ControlApi` + route, with gRPC mirrored or
   a `SURFACE_GAPS` entry): `issues.list` / `issues.get` (Read),
   `issues.update` / `issues.comment` (Write) over the existing `IssueRouter`;
   `dispatches.list` (Read), `dispatches.put` / `dispatches.set_status`
   (Write) over the `agent_dispatches` roster; `worktrees.create` (Git) over
   `worktree::add_checked`, accepting an optional issue id that derives the
   branch from the tracker's `branch_hint` and links the issue. MCP tool names
   fall out of `CapId::tool_name` for free.
3. **CLI verbs** so the loop is drivable without MCP and testable from
   `test/smoke.sh`: `thegn session open --agent <name> --prompt <p> --worktree
<w> [--headless] [--bind]`; `thegn wt new --from-issue <id>`;
   `thegn dispatch list|set-status --json`; `thegn issue list --status
--limit`.
4. **Fix the dispatch-status drift**: add `Done`/`Failed` variants and
   `AgentDispatchStatus::parse`; route both writers (`pty_drain.rs` and the
   tracker handler) through the enum; tolerate the legacy strings on read. No
   schema bump (TEXT column).
5. **`TaskKind::Issue`** in the agent-task engine (prompt vars
   `issue_number/issue_title/issue_body/issue_url/branch/worktree`, a default
   prompt), so dispatched workers get a rendered prompt with the engine's
   quoting contract, watchdog, and git-env scrub — and the `D` key stops
   hardcoding `"claude"` (resolve the configured agent instead).
6. **The supervisor skill** (`extensions/skills/`, beside `tui-check/`)
   encoding the conductor loop: discover issues → create worktree → record
   dispatch → open session → wait (always with a timeout) → on `blocked`
   snapshot-and-decide → on `done` take the run's configured exit (enqueue /
   PR / issue transition / stop) → resume from `dispatches.list` after a crash,
   never re-dispatching a `Running` row.

## Impact

- **Roadmap**: Q 211/212/215/224 (create task, task→worktree→agent pipeline,
  queue, batch launch — realized as _tool surface + skill_, not a driver; the
  old embedded-orchestrator framing of group Q is superseded), Q 216/217
  (follow-up / answer a waiting agent — via `sessions.input` + the blocked
  state), Q 223 (task history — the durable roster), AL 462
  (`wait_for_task` — `sessions.wait` projected over MCP), AA (Linear write
  verbs), S 256 sibling (attention surfacing).
- **Specs**: `agent` (ADDED: launch-by-name, issue task kind),
  `control-plane` (ADDED: session activity + wait, tombstones, orchestration
  capability rows), `cli` (ADDED: session open + dispatch/issue verbs),
  `state-db` (ADDED: the dispatch roster contract, statuses a closed parseable
  set).
- **In-flight changes**: depends on `add-agent-task-engine` (TaskKind::Issue
  extends it; do not fork its resolution) and the MCP write-tools branch
  (`--scopes` gating for the new Write tools; do not re-scope it). Coordinates
  with `add-issue-driven-worktrees` (the TUI `s`-key sibling — same underlying
  pipeline, this change adds the headless/catalog door), `add-generic-tracker-model`
  (issues.\* rows ride `IssueRouter`; if that change reshapes the model the rows
  follow), `make-daemon-default` (supervision requires the daemon),
  `add-osc-attention-signaling` (the authoritative blocked/attention signal the
  FSM consumes). **Flags `add-fleet-view` for archive/rework**: it depends on
  the excised LLM proxy and holds the `fleet` verb name; nothing here claims it.
- **AI-free shell**: strictly additive. Every row works with no agent
  configured (`issues.*`/`worktrees.create` are plain tracker/git operations;
  `dispatches.*` is an empty roster; `session open --agent` errors honestly).
- **No DB schema change**: `agent_dispatches` (v12) already exists; the status
  fix is a value-domain fix, not DDL.
