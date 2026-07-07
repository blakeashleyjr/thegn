# Design

## Overview

Three cohesive capabilities, designed to compose: a **pipeline** modeled as a
**workflow graph** of typed nodes wired by edges and executed in an ephemeral
worktree, a **review-gate** that models the findings those nodes produce and how
they are resolved (attached as node-level policy), and **change-intent** that
gives the review/PR a deterministic statement of what the change was for.

The linear pipeline that `no-mistakes` inspired is **not gone** — it is the
**default graph instance**: when the user configures nothing, the engine
instantiates the exact chain intent → review → test → lint → document →
approval → PR, so behavior is identical to a fixed pipeline until someone edits
the TOML.

## The workflow graph

### Node kinds

Every node has a kind, an id, and kind-specific policy:

- **`agent-exec`** — an AI stage (change-intent capture, AI review, change
  explanation). Requires an agent/proxy; **skipped** when AI is absent.
- **`check`** — a deterministic command node with a `check` sub-kind in
  {`test`, `lint`, `fmt`}. Always runnable (this is the AI-free floor).
- **`approval-gate`** — a pause node that parks the run awaiting a resolution
  (approve / fix / skip). This is where the finding model's `ask-user` findings
  collect. Can hand parked findings to `add-agent-steerable-review`'s panel.
- **`pr`** — opens/updates the pull request via the existing `superzej pr create`
  path, using change-intent for the body.

### Edge kinds

Edges wire nodes and carry the control flow:

- **`sequence`** — run the target after the source completes (the spine of the
  default graph).
- **`conditional-on-severity`** — traverse only if the source's findings meet a
  severity predicate (e.g. route to an `approval-gate` only when there is a
  `warning`/`error`, or when the blast-radius risk score from
  `add-semantic-blast-radius` crosses a threshold). This is the input point for
  T 266's risk score.
- **`parallel`** — fan out to multiple targets that run concurrently (e.g. test +
  lint), joined before the next node. **Modeled now, shipped later.**
- **`loop(fix→re-review ≤N)`** — cycle back to a review node after a fix node, at
  most `N` times (mirrors the finding model's `auto_fix_limit` bound at the graph
  level). **Modeled now, shipped later.**

### TOML authoring (layered config, `config_enum!` idiom)

The graph is authored in superzej's layered TOML, node kinds/edge kinds are
`config_enum!` variants, and the whole `[pipeline]` table is optional:

```toml
[pipeline]                       # entire table optional -> default linear graph
entry = "intent"

[[pipeline.node]]
id    = "review"
kind  = "agent-exec"             # agent-exec | check | approval-gate | pr
auto_fix_limit = 0               # node-level finding policy (review default 0)

[[pipeline.node]]
id    = "test"
kind  = "check"
check = "test"                   # test | lint | fmt

[[pipeline.node]]
id    = "gate"
kind  = "approval-gate"

[[pipeline.edge]]
from  = "review"
to    = "gate"
kind  = "conditional-on-severity"   # sequence | conditional-on-severity | parallel | loop
when  = "warning"                   # severity predicate (for conditional)
```

When `[pipeline]` is absent, the engine loads a **built-in default graph** equal
to the linear chain (each stage a `sequence` edge, review→gate a
`conditional-on-severity` edge, gate→pr a `sequence` edge). No TOML ⇒ no behavior
change.

## The executor is a pure state machine

The engine is a **pure state machine** over the node graph, the same shape and
ethos as `render_plan::plan`:

- Input: the graph (nodes + edges), the current run state (which nodes are
  done/parked/failed, accumulated findings), and an injected event (a node
  result, a finding, a resolution). Output: the next set of nodes to run (or
  `Park`, or a terminal `Done`/`Failed`).
- **All I/O is injected** — running a check command, invoking the agent, creating
  the worktree, persisting to SQLite — so the state machine itself is
  deterministic and side-effect-free. It lives in `superzej-core`, is
  exhaustively **unit-tested**, and is **coverage-gated** (95% lines) exactly like
  `render_plan`.
- Determinism gives us a regression gate analogous to render-plan's: given the
  default graph and a fixed event stream, the node-visit order is asserted to be
  the linear chain; a graph with a `conditional-on-severity` edge is asserted to
  branch to the gate only on a qualifying finding.

## Why not copy the `no-mistakes` transport

`no-mistakes` intercepts `git push` via a bare gate repo + a pinned post-receive
hook, orchestrated by a background daemon. superzej is **already** the
long-running compositor and owns the UI, so none of that is needed:

- The graph is triggered by a panel/palette action ("Validate branch") or
  automatically on agent-task completion.
- There is no second remote, no `core.hooksPath` pinning, and no
  systemd/launchd/schtasks service — those would be redundant moving parts that
  fight superzej's one-process / one-session model.

## Render impact

- Review-gate findings surface in the **existing diff/review pane** (T 260) and
  in **`Section::Problems`** — both already part of the chrome.
- An `approval-gate` node parking (awaiting a decision) is a chrome state change
  ⇒ it triggers a `Full` frame **only when the gate state changes**, never per
  node transition. This respects the damage-region invariant: idle ⇒ `Skip`, pane
  output ⇒ `Panes`, chrome/overlay change ⇒ `Full`.
- The graph runs **off the loop** on `spawn_blocking`; each node transition and
  finding sends on a tokio mpsc channel and pulses the `TerminalWaker`, and the
  loop re-renders only when dirty. **No blocking I/O on the loop and no polling
  timeout** — the run does not introduce a tick. (Parallel nodes, when shipped,
  are multiple `spawn_blocking` tasks whose results are still delivered over the
  same channel + waker — no new loop tick.)

## Ephemeral worktree

- Created via `GitBackend` under a reserved naming scheme (e.g. a
  `.szpipeline/<run-id>` prefix) and **not** registered as a sidebar tab, so it
  never appears in the workspace tree.
- git remains the source of truth; the worktree is on the host like any other.
- GC'd on run completion (success, failure, or cancel) by reusing the existing
  stale-worktree cleanup seam (`worktree::clean_target`, roadmap item 48). A
  crashed run leaves a reserved-prefix worktree that the same GC reclaims.

## State / DB

- A cache table records graph runs, their node states, and findings (mirrors the
  runs/steps/findings shape `no-mistakes` keeps in SQLite), so a parked
  `approval-gate` survives a restart and the review pane can rehydrate.
- This is a **cache + resurrection** layer only — git and the agent session
  remain authoritative. Adding the table **requires a `user_version` bump**.

## Finding model (as node-level policy)

The finding model is preserved verbatim and attached to nodes:

- `severity ∈ {info, warning, error}` × `action ∈ {auto-fix, ask-user}`.
- Per-**node** `auto_fix_limit`: how many `auto-fix` findings a node may apply
  automatically. **The review node defaults to `0`** ⇒ blocking/`ask-user` review
  findings **park** at the downstream `approval-gate` rather than self-applying.
- `info` mechanical findings (format/lint/import-sort/doc-stub) auto-apply — this
  preserves the current "edits auto-apply by design" default for the safe class
  while carving out the intent-touching class for explicit review.
- Resolution actions: **approve** (accept as-is), **fix** (apply the suggested
  change), **skip** (drop the finding). Surfaced to the human via the review pane
  (and `add-agent-steerable-review`'s panel) and exposed to superzej's **own
  embedded agent** through an ACP-shaped structured contract (aligns with R 232) —
  not a separate standalone CLI.
- `conditional-on-severity` edges read the same severity, so the graph's routing
  and the node's auto-fix policy stay consistent.

## Change-intent

- Derived from the **agent-session↔worktree binding** superzej already maintains
  (the embedded agent runs bound to a worktree; sessions persist/resurrect). The
  intent is the agent's task/prompt for the session that produced the change —
  available directly, with **no transcript-scraping or file-overlap heuristics**.
- When no bound agent session exists (e.g. a hand-edited worktree), intent is
  simply absent; the review/PR proceeds without it (degrade, don't fail).
- Consumed by review/gate nodes (findings are judged against intent) and by the
  `pr` node (generates the intent/changes/risk/evidence sections of the PR body),
  reusing the existing `superzej pr create` path.

## AI-additive invariant

The graph MUST run with AI strictly additive:

- With no agent/proxy configured, the engine runs only the deterministic `check`
  nodes — the configured `test` / `lint` / `fmt` commands plus pre-commit hooks —
  and **skips every `agent-exec` node** (intent / AI review / change explanation).
  The default graph with AI absent collapses to test → lint → (approval) → PR.
- The shell never hard-depends on the AI layer; `agent-exec` nodes enrich the
  graph but are never required for it to run or to open a PR.
