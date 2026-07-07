# Add agent review-gate pipeline

## Summary

Adopt three ideas — validated against the external tool
[`no-mistakes`](https://github.com/kunchenguid/no-mistakes) — that give shape to
superzej's planned-but-unstarted agent review/merge work, but generalize the
fixed stage chain into a **workflow graph** authored in layered TOML:

1. **Agent validation as a workflow graph in an ephemeral worktree** — a graph of
   typed **nodes** (`agent-exec | check | approval-gate | pr`) wired by **edges**
   (`sequence | conditional-on-severity | parallel | loop`), executed in a
   throwaway, non-tab worktree so the user's working directory and open tabs are
   never disturbed. The graph is authored in TOML (`[[pipeline.node]]` /
   `[[pipeline.edge]]`) using superzej's `config_enum!` layered-config idiom.
   **When unconfigured, the default graph is exactly today's linear pipeline**
   (intent → review → test → lint → document → approval → PR), so nothing
   regresses — the linear chain is just the default instance of the graph model.
2. **Review-gate finding model** — findings carry a `severity` × `action` pair
   with a per-node auto-fix limit, so mechanical fixes apply automatically while
   anything that touches intent _parks_ at an approval-gate for an explicit
   approve / fix / skip decision. The finding model is preserved verbatim and
   attached as **node-level policy** on the graph.
3. **Change-intent attached to review/PR** — "what this change was trying to do,"
   derived deterministically from superzej's native agent-session↔worktree
   binding (not by scraping transcripts), surfaced in review and used to generate
   the PR body.

The transport layer that `no-mistakes` uses (a bare gate repo, a pinned
post-receive hook, and a background daemon) is deliberately **not** adopted:
superzej is already the long-running compositor and owns the UI, so it triggers
the graph from a panel/palette action or on agent-task completion.

The executor is a **pure state machine** over the node graph — same shape and
ethos as `render_plan::plan` — with I/O injected at the edges so it is
deterministic, unit-tested, and coverage-gated in `superzej-core`.

## Impact

Roadmap items (tasks.md) this change gives concrete behavior to:

- **Q 211** — Create task (prompt/spec)
- **Q 212** — Task→worktree→agent→review→merge pipeline
- **T 262** — Inline comments → follow-up prompt
- **T 263** — Approve→merge / reject→discard
- **T 266** — AI change explanation (sem + LLM) — the blast-radius risk score from
  the new change `add-semantic-blast-radius` feeds a review/gate node as input
- **T 269** — PR creation from review
- **AR 581** — Eval hooks gate any risky transform

Relates to (should be ACP-shaped rather than a parallel mechanism):

- **R 232** — ACP permission requests → UI
- **R 233** — ACP diff rendering into the review pane (T 260)

Cross-change relationships:

- **`add-semantic-blast-radius`** — the blast-radius risk score is an **input to a
  review/approval-gate node** (drives conditional-on-severity edges; T 266).
- **`add-agent-steerable-review`** — its interactive review panel can **receive
  parked findings** from an approval-gate node for human steering.

New capabilities introduced (ADDED specs): `agent-pipeline`, `review-gate`,
`change-intent`.

## Rationale

- **A graph, not a hardcoded chain.** Users need to reorder/skip/parallelize
  stages (run test+lint concurrently, loop fix→re-review, gate on risk) without a
  code change. Modeling the pipeline as TOML-authored nodes+edges — with the
  linear chain as the default instance — makes control flow data, keeps the
  executor a small pure state machine, and matches superzej's existing
  layered-TOML config idiom (`config_enum!`).
- **superzej is _inside_ the agent.** `no-mistakes` works hard to _recover_
  intent from outside (transcript readers + file-overlap scoring + disambiguation).
  superzej already binds agent sessions to worktrees, so intent attaches
  deterministically and cheaply — the highest value-to-effort transfer.
- **The ephemeral worktree keeps the user undisturbed.** superzej is
  worktree-native and already spins per-worktree sandboxes; running the graph in a
  reserved, non-tab worktree is a natural fit and reuses the existing
  stale-worktree GC seam.
- **The finding model is the missing middle ground.** Today the agent edit path
  is binary (auto-apply by design); the review-gate is manual-only. `severity ×
action` with an auto-fix limit keeps mechanical fixes automatic (the current
  default for `info`) while parking intent-touching changes for a decision. It
  survives the reframe as per-node policy.

## Non-goals

- No bare gate repo, pinned `core.hooksPath`, or background daemon — redundant
  with the single-process compositor model.
- **No DOT / Graphviz or a bespoke graph DSL** — the graph is plain layered TOML
  (`[[pipeline.node]]` / `[[pipeline.edge]]`), consistent with the rest of
  superzej config; no new file format is introduced.
- No heuristic transcript-scraping for intent — only relevant when the tool lives
  outside the agent.
- No AI hard-dependency: the graph MUST degrade to the non-AI nodes (test/lint/
  format check nodes + pre-commit hooks) when no agent/proxy is configured; AI
  (`agent-exec`) nodes are skipped, not required.
- Not shipping parallel/loop control flow on day one — the **model** defines them,
  but the first cut ships sequence + approval-gate + conditional; parallel and
  loop land as follow-on tasks.
