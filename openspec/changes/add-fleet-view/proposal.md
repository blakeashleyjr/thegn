# Add fleet view (btop-for-agents, authoritative metrics)

## Summary

Add a rich per-worktree agent-metrics surface — a "btop for agents" modeled on
[`abtop`](https://github.com/graykode/abtop) — that shows, for each worktree
running an agent: context-window %, token split (input / output / cache-read /
cache-create), a token-rate sparkline, **compaction detection** (a context drop
between turns), turn count, current task, child processes + their open ports, and
a live **tool-call timeline** (a Thinking/Executing row that grows as work
happens). A `thegn fleet --json` snapshot exposes the same model to external
tools. Where abtop can only scrape agent transcripts, thegn sources the token
metrics **authoritatively through the LLM proxy**.

## Impact

- **S 244** (abtop-style fleet view) — this is the direct realization of that
  roadmap item, using abtop's data schema.
- **S 251/252/253** (activity heuristics) — the fleet view presents authoritative
  proxy-sourced metrics alongside the existing activity states.
- **S 256** — surfaces per-agent state richly for the needs-attention flow.
- Extends the `agent` capability and reuses the `sidebar`/chrome model. **No DB
  schema change** — metrics are a read-time aggregation over the existing
  `proxy_requests` audit table; the only proxy-side addition is extracting cache
  tokens already present in provider responses.

## Rationale

thegn's per-worktree indicator today is a heuristic activity dot (CPU-derived).
abtop shows what a rich row looks like and proves the value. thegn is uniquely
positioned to do it _better_: the LLM proxy already writes a `ProxyRequestRow`
(input/output tokens, cost, agent, worktree) per request, so token/context metrics
are **authoritative**, not scraped. The proxy just needs to also parse the cache
token fields that Anthropic responses already carry. The live tool-call timeline
is a bounded-diff `Incremental` update under the render invariants — cheap. abtop
also teaches a discipline worth keeping: the monitor is **read-only** and never
spends quota to render.

## Non-goals

- **Orchestration from the fleet view** — it observes; it does not launch, kill,
  or send prompts (observe ≠ orchestrate). Fan-out/best-of-N is the separate
  team-fanout change; the fleet view may later link to those actions.
- **A generic metrics/dashboards client** — that is `add-observability-dashboards`
  (Prometheus/Loki/SQL). The fleet view is agent-scoped and may later expose its
  model as a `host`-DataSource panel there.
- **Spending tokens to render** — the snapshot/rollup is derived from stored
  request rows and live process state only; it never issues a model call.
- **AI-free-shell dependency** — with no agent running, the fleet view is empty;
  the shell does not depend on it.
