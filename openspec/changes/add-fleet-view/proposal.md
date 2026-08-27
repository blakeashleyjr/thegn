# Add fleet view (btop-for-agents, authoritative metrics)

> **⛔ STATUS 2026-08-26 — SUPERSEDED by `openspec/changes/add-pipeline-board`.**
> Do not implement this change. Its blocked dependency (the excised LLM proxy)
> is back as `resurrect-model-proxy`, but its _framing_ did not survive: the
> agent-orchestration work reserved the `thegn fleet` noun, and the durable
> `agent_dispatches` roster — not a scraped per-agent metrics rollup — turned out
> to be the right spine for a live agent surface.
>
> **Carried into `add-pipeline-board`:** the load-bearing render invariant
> (a live agent surface is an `Incremental`/`Panes` bounded diff, **never** a
> Full chrome recompose — design.md:49-53 below), the off-loop-hydration rule,
> and the additivity rule (no agent ⇒ empty surface).
>
> **Dropped:** the `fleet` noun and the `thegn fleet` CLI verb entirely.
>
> **Deferred to that change's phase 2:** the per-agent token / context-% / cost
> / compaction / tool-timeline metrics. They are now a groupby over
> `model_proxy_requests` (`store/model_proxy.rs::model_proxy_requests_since`;
> cache-token columns already exist at `db_model_proxy.rs:31-32`) — data that has
> landed, so this is a column set on an existing board rather than a change of
> its own.

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
