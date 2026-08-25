# Resurrect the model proxy — tier routing, failover, cost accounting

Linear: THE-58

## Why

Agent CLIs launched from thegn each talk to their model provider directly: one
provider outage stalls every pane, there is no way to steer cheap tasks onto
cheap models, and spend is only visible per-account (the `[usage]` quota
tracker), never per-request, per-worktree, or per-route. thegn already solved
this once: the pre-alpha `thegn-proxy` (`tgproxy`) was a dual-protocol
(Anthropic + OpenAI) local endpoint with ordered failover, per-lane health
cooldown, rate limiting, streaming relay, and per-request spend attribution. It
was excised end-to-end for the 0.1.0-alpha.1 public alpha (`85f3d1fb`,
`bcb6af8a`, `0e009b7a`) because the alpha ships as the AI-free workspace shell —
"deferred, not cancelled".

This change resurrects the proxy **as an opt-in, additive subsystem** with the
excision's lessons baked in: a fresh `[model_proxy]` config section (never the
stale, warned-and-dropped `[llm_proxy]` name), fresh DB table names (the orphaned
`proxy_*` tables stay untouched per the shared-DB contract), provider entries as
a proper seam with SecretRef-only key custody, and integration with the
`[usage]` per-account tracker that shipped after the excision. Everything
ACP/bouncer-shaped — the sealed-container unix-socket bridge, tool
interception, the managed agent — stays dead.

## What Changes

- **Revive `crates/thegn-proxy`** (binary `tgproxy`): the axum I/O shell —
  server, cascade router, streaming relay (OpenAI SSE + Anthropic SSE with
  cross-translation), health/cooldown state, upstream dispatch. Restored from
  git history at `85f3d1fb^` and adapted, not rewritten.
- **Restore the pure logic to `thegn-core::proxy`** under the 95% coverage
  gate: response classification (`Serve`/`Soft`/`Exhausted`), cost estimation +
  price table, multi-key credential pools, token-bucket rate limiting, request
  transforms, stats rollups — plus two new pure modules: a deterministic
  **tier classifier** (the `auto` route alias: pick a tier from request
  features) and **usage-aware lane ordering** (deprioritize/skip lanes whose
  account is near its `[usage]` window cap).
- **New `[model_proxy]` config section**: TOML-native provider registry
  (`[[model_proxy.providers]]`, `kind` implemented-or-reserved, `api_key`
  SecretRef-only) and route/tier table (`[[model_proxy.routes]]` + `aliases`),
  routing strategy (`sequential` / `load_balanced` / `cost_aware`), streaming
  timeouts, optional per-scope budgets — replacing the old side-JSON routes
  document with layered, schema-validated, trust-clamped thegn config.
- **New DB tables** `model_proxy_requests` (metadata-only audit rows incl.
  cache-token fields) and `model_proxy_budget_state`, additive migration +
  `user_version` bump. Legacy orphaned `proxy_*` tables are never reused,
  migrated, or dropped.
- **Host lifecycle**: launch + supervise `tgproxy` when enabled (listen socket
  is the lock; `core::backoff` restarts), `thegn proxy
status|stats|start|stop|serve` CLI, four new capability catalog rows
  (`model_proxy.status/stats/start/stop`) on the OPERATOR surfaces with
  `required_scope` policy, a `thegn doctor` probe, and opt-in per-agent env
  injection (`[[agents]] route_via_proxy`) that probes the proxy before
  injecting so a down proxy can never strand an agent.
- **`[usage]` integration**: the System ▸ Usage panel and usage dashboard gain
  a proxy-spend block (cost/tokens by route, off-thread reads); budget breaches
  surface through the existing usage-alert/notification path; account-window
  headroom optionally feeds routing.

## Impact

- **Roadmap**: resurrects group **U. LLM proxy (271–288)** — dual-protocol
  relay (271), configurable upstreams (272), tier aggregation (273), ordered
  failover (274), exhaustion/reset tracking (275/276), cooldown/failback
  (277/278), key load balancing (280), aliasing (281), auto-downgrade (282),
  local upstreams via openai-compat (283), streaming passthrough (285), virtual
  attribution keys (287), daemon management (288) — and group **V. Cost /
  limit / budget (289–299)** — cost logging (289), spend attribution (290),
  budget caps + enforcement (292/293/295), spend history/stats (298/299).
  Group **W (token reduction)** is explicitly out of scope. V 300 (`[usage]`)
  is live on main and is extended, not replaced.
- **Specs**: new `model-proxy` capability; `state-db` — ADDED the
  `model_proxy_requests` / `model_proxy_budget_state` tables. Every new
  externally invokable operation is a `thegn_core::capability::CATALOG` row
  gated by `required_scope(verb)` — no second policy table.
- **Code**: revived `crates/thegn-proxy`; `thegn-core/src/proxy/` +
  `config_model_proxy.rs` + `db_model_proxy.rs` + store trait;
  `thegn-svc/src/model_upstream.rs` (the upstream seam impls);
  `thegn-host/src/{model_proxy_daemon.rs, cmd/proxy.rs}`, usage-panel spend
  block, agent-spawn env injection.
- **DB schema change**: `user_version` bump (additive tables only).
- **Help**: `docs/help/ai-usage.md` gains the proxy-spend block prose; the
  generated config-reference and keybindings pages pick up `[model_proxy]`
  automatically; no new actions, keybinds, zones, or panel sections (the spend
  block extends the existing `panel:usage` context), so no help-ratchet debt.
- **Related in-flight changes**:
  - `add-fleet-view` was written pre-excision and assumes the excised
    `proxy_requests` table. This change re-provides authoritative per-request
    token/cost rows (including cache tokens) under `model_proxy_requests`;
    fleet-view must re-target those rows when it is picked up — noted here,
    not edited there. This change does not claim the `fleet` verb.
  - `add-agent-task-engine` / the queues' agent handoff: a handoff subprocess
    benefits from `route_via_proxy` env injection like any `[[agents]]` entry;
    no coupling — both work with the proxy absent.
  - `make-daemon-default`: independent. The proxy is supervised by the host
    per state dir, not by the pane daemon (see design.md Decisions).
  - The in-flight MCP write-tools/scope-gating branch: no interaction — the
    proxy verbs claim only the OPERATOR surfaces (HTTP/gRPC/CLI), never MCP or
    plugin.
- **AI-free-shell guarantee**: with `[model_proxy]` absent or disabled, no
  proxy process launches, no thread spawns, no DB row is written, and no UI
  element renders. Every queue, panel, and agent launch works exactly as
  today. If the resurrection is later re-excised, the shell loses nothing.

## Non-goals

- **ACP / bouncer / tool interception** — the sealed-container unix-socket
  bridge (`proxy/bridge.rs`), the allow/deny overlay, and the managed-pi stack
  stay dead. If the agent-harness track (unit G6a) needs a tool-gating seam,
  that is its scope to claim; this change deliberately does not restore it.
- **Token reduction / compression (group W)** — the old `compress.rs` /
  `token_reduction` knobs are not restored. External tools (llmtrim, rtk)
  compose with the proxy via standard env chaining; a native engine is a
  separate future change.
- **Remote-sandbox reachability** — the old `remote_base_url` reverse-tunnel
  injection is deferred; this change is local-first (loopback + local agent
  env injection). The revtunnel machinery that remains on main (git-cache) is
  untouched.
- **A proxy dashboard overlay** — the excised `Ctrl Alt l` TUI dashboard and
  its action/keybind are not restored; stats surface via CLI/`/stats` and the
  usage panel's spend block.
- **The `thegn-agent` iroh call-home dialer** — retired with its transport;
  not part of the proxy.
