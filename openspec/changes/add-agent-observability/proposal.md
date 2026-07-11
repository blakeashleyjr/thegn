# Add agent observability ("Agents")

## Summary

Surface what every coding agent is doing — across all worktrees — **without
depending on the LLM proxy or any network API**. The whole feature is a
_local-read_ observability layer: parse on-disk provider state and the
terminal's OSC title, list and resume past transcript sessions, and stream a
cross-worktree agents feed over the existing `EventBus`. Concretely:

- a **local usage/rate-limit reader** that parses `~/.claude` / `~/.codex`
  on-disk state (plan %, reset windows, 80% warning) and shows it as a
  statusbar widget — so it works even for agents that bypass the proxy;
- **OSC-title agent-state detection** that makes the terminal title the
  _authoritative_ working / idle / waiting signal feeding the activity dots and
  the agent chip, with the existing CPU heuristics as a fallback;
- a **cross-provider session-history backend** that scans native transcript
  dirs (`~/.claude`, `~/.codex/sessions`) and offers one-click resume via the
  agent's own `--resume` command;
- an **agents feed** — a threaded, cross-worktree activity log over the
  `EventBus`, with running agents smart-pinned and click-to-jump to their pane.

## Impact

- **S** (agent observability), items **759–762**: 759 local usage/rate-limit
  reader, 760 OSC-title agent-state detection, 761 cross-provider session
  history + one-click resume, 762 agents feed.
- Complements **S 658** (agent hibernation — the feed/state model surfaces what
  can be hibernated) and **257** (existing activity-dot work).
- Feeds **L** (statusbar widgets) items **148/149/150/157**: the usage widget,
  the agent-state chip, and the agents-feed badge are L-group statusbar/chrome
  consumers.
- New capability: `agent-observability`.

## Rationale

The activity-dot state machine, the per-provider `account.rs` registry, the
`hydrate.rs` off-loop refresh seam, the `EventBus`, and the panel `Section`
accordion already exist. Agent observability rides all of them: provider state
and OSC titles are _read locally_ (no API, no proxy round-trip), refreshes run
**off the event loop** on the existing ticker and wake the loop on a result so
the 0%-idle contract holds, and the read-only model stays in `thegn-core`
behind testable parsers. Because every signal is read from disk or the terminal,
the AI-free shell gains the observability surface as pure additive chrome —
nothing here becomes a hard dependency on the AI/proxy layer.

## Non-goals

No network calls / no provider API polling (rate state is read from disk only);
no editing or mutation of provider state files; no proxy dependency; no agent
_orchestration_ or dispatch (that is the AI layer, item 658 et al.); no
cross-machine aggregation of remote-env agents beyond what the existing bridge
already reports.
