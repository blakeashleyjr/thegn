# Design

## Metrics source (authoritative, via the proxy)

The proxy's `finalize_success()`/`parse_usage()` path already persists a
`ProxyRequestRow` (input/output tokens, cost, agent, worktree, timestamp) per
request. Two additions:

1. **Cache tokens** — `parse_usage` also extracts Anthropic's
   `cache_creation_input_tokens` / `cache_read_input_tokens` (present in responses,
   not parsed today) so the split is complete.
2. **Per-worktree aggregation** — a read-time query rolls `proxy_requests` up by
   worktree over a window (turn count, token totals, token-rate for the sparkline).

No model call is ever issued to render (abtop's "never spend quota" discipline).

## FleetMetrics carrier (host)

`chrome.rs` `FrameModel` carries `ai_metrics: Option<AiMetrics>` for the active
worktree today; add `per_worktree_metrics: HashMap<String, FleetMetrics>` where
`FleetMetrics { context_pct, tokens: TokenSplit, token_rate_history, turns,
compactions, current_task, children: Vec<ChildProc>, tool_calls: Vec<ToolCall> }`.
It is hydrated off-loop on a new `RefreshKind::Fleet` ticker (the hydrate pattern),
handed back over the channel + `TerminalWaker`.

## Compaction detection (core, pure)

A pure function flags a compaction when a turn's context tokens drop by more than
a threshold (e.g. >30%) versus the prior turn — **unit-tested** (drop over
threshold flags, small dip does not, first turn never flags, monotone growth never
flags). Runs over the per-worktree token history.

## Tool-call timeline + orphan ports

The tool-call timeline model is fed from ACP tool-call events (start/end,
name, duration) the agent layer already surfaces; a live "Executing" row grows
until its end event. Child-process ports come from the existing `forward.rs`
`ForwardEvent::Detected` detector; a port whose owning process has exited is
flagged as orphaned.

## Surfaces

- A fleet **panel/overlay** rendering the per-worktree rows + the animated
  timeline for the focused agent.
- `thegn fleet [--json]` CLI subcommand (the `cmd/issue.rs`/`config.rs` `--json`
  pattern) emitting the rollup for external tools — read-only, quota-free.

## Invariants

- **Event loop**: metrics hydrate off-loop on a ticker + channel + waker; no
  blocking query on the loop, no busy polling.
- **Render**: the live tool-call/token strip is an `Incremental { bars }` /
  `Panes`-class bounded-diff update — **never** a Full chrome recompose. This is
  the load-bearing render invariant for this change; a render test locks it.
- **State**: no `user_version` bump — aggregation is read-time over existing
  `proxy_requests`.
- **Additivity**: no agent ⇒ empty fleet; the shell never depends on it. Cache
  parsing lives in the proxy (AI layer).

## Alternatives considered

- **Scraping agent transcripts (abtop's approach)** — rejected; the proxy gives
  authoritative counts without guessing, and works for any harness routed through
  it.
- **A stored per-worktree metrics table** — rejected; read-time aggregation over
  the existing request log avoids a schema bump and can't drift from the source.
- **Recomposing chrome each metrics tick** — rejected; violates the render
  invariant. The strip must be a bounded diff.
