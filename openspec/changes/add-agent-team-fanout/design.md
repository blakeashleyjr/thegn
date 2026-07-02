# Design

## The verb

```
szhost team <task> \
  --agents claude,codex,gemini   # one worktree+agent per entry (heterogeneous)
  # or
  --best-of-N 3 --agent claude   # N isolated attempts of the same task
  [--base <branch>] [--sandbox <backend>] [--label <name>]
```

- **Heterogeneous mode** (`--agents a,b,c`) creates one worktree per agent, each
  on its own branch, and launches that agent with the shared task prompt.
- **Best-of-N mode** (`--best-of-N k --agent a`) creates `k` worktrees on `k`
  branches all running the same agent + prompt — the deferred Q 225 pattern.

The caller's current pane becomes the **orchestrator** (kept, not replaced);
teammates open as sibling panes in a layout.

## Composition (reuses existing primitives)

- **Worktrees**: each teammate calls the existing worktree-create path (D 41–43),
  with the branch-name template extended by a team label + index
  (`team/<label>/<agent-or-idx>`).
- **Sandbox**: each teammate's process enters the sandbox via the existing
  `sandbox::enter_argv` seam; `--sandbox` overrides the backend. The **warm
  sandbox pool** is the natural accelerator — a team of N pulls N warm spares when
  available (decide_pool), so fan-out is near-instant.
- **Panes/layout**: teammates are laid out with the existing `CenterTree` split
  ops; the team is a grouping over the resulting worktree tabs (a `team_label` on
  the session/worktree grouping, not a new table).
- **Agent launch**: reuses `pick_agent` / the agent-launch seam per worktree.

## Coordination model (visibility inversion)

Teammates are **visible panes**, never background processes — the orchestrator
(the human, or an agent in the orchestrator pane) observes teammate panes and
their activity dots / attention signals (this composes directly with the
OSC-attention-signaling change: a teammate that finishes or blocks raises its
hand). No hidden tmux-style background agents.

## Fleet view (S 244)

The team grouping renders as a fleet roster (sidebar section or panel): one row
per teammate showing branch, activity/attention state, and (via the proxy) live
tokens/cost when available. This is the first real consumer of the abtop-style
fleet view; it reuses the sidebar row + activity-dot rendering.

## Review & merge (T)

Teammate branches are ordinary worktree branches, so the **existing** diff/review
pane (T 260), needs-attention jump (T 259), cycle-through-diffs (T 267), and
approve→merge / reject→discard (T 263) work unchanged. `--best-of-N` simply
presents N sibling diffs to compare; picking one is a normal merge, discarding
the rest is a normal worktree cleanup (D 47/56).

## Invariants

- **Event loop**: worktree creation, sandbox warm-up, and agent launch are
  off-loop (`spawn_blocking` / existing async paths) that report back over the
  mpsc channel + `TerminalWaker`; no blocking git/subprocess on the loop, no new
  timer.
- **Render**: the fleet roster and new panes are chrome/geometry changes → a
  `Full` frame on team creation (geometry), then per-teammate pane output is
  ordinary `Panes` diffs. render_plan invariants unchanged.
- **State**: no `user_version` bump — a team is a set of worktrees plus a
  `team_label` grouping on existing rows; git remains source of truth.
- **Additivity**: the fan-out primitive (create N worktrees, run a program in
  each) is AI-free; only the default program (an agent) makes it "agent team".

## Alternatives considered

- **Shared-checkout parallel agents (cmux/limux model)** — rejected: parallel
  edits collide. Worktree-per-teammate is exactly the isolation we already have.
- **Background (hidden) agents** — rejected on the visibility-inversion principle;
  every teammate is a visible, inspectable pane.
