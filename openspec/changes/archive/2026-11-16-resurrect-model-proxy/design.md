# Design — resurrect-model-proxy

## Context

The pre-alpha proxy was removed in three commits (`0e009b7a` dashboard,
`bcb6af8a` daemon/CLI/env-routing, `85f3d1fb` the subsystem end to end —
~12,000 lines). The removal is clean and well-documented, so **git history at
`85f3d1fb^` is the reference implementation**: `crates/thegn-proxy/src/`
(router, relay, anthropic_stream, server, health, budget, model, upstream) and
`crates/thegn-core/src/proxy/` (classify, cost, creds, ratelimit, transform,
stats, compress, bridge). What the old proxy did:

- **Dual-protocol endpoint** — served OpenAI `/v1/chat/completions` and
  Anthropic `/v1/messages`, translating either wire surface onto either kind
  of upstream, streaming SSE with first-byte peek, idle kill, heartbeats.
- **Cascade routing** — named routes (tiers) of prioritized backend lanes;
  `aliases` mapped client model ids onto routes; strategies `sequential`,
  `load_balanced`, `speculative` (cheapest-first); optional cross-route
  `last_resort` tier.
- **Health / failover** — every response classified `Serve` / `Soft`
  (fall through, no cooldown) / `Exhausted` (429/402/auth/credit/5xx → cool
  the lane down, honoring `Retry-After` / reset windows).
- **Multi-key lanes** — per-provider credential pools with
  roundrobin/failover/random/weighted ordering; identity-scoped token-bucket
  rate limiting and in-flight caps.
- **Accounting** — a `proxy_requests` audit row per request (tokens, cost,
  latency, TTFB, route/backend, caller scope), a price table with
  subscription-lane $0 attribution, stats rollups (spend, tokens/sec,
  p50/p95) shared by `/stats`, CLI, and the (excised) dashboard.
- **Budgets** — per-scope $/token ceilings with rolling windows,
  refuse-or-downgrade on breach, kill-switch.
- **Attribution keys** — per-worktree/workspace virtual keys carried caller
  scope and upstream-account pinning.

Since the excision, two relevant things shipped on main: the **`[usage]`
per-account quota tracker** (V 300 — provider-side rate-limit windows per
discovered account, statusbar gauge, System ▸ Usage panel, alerts, SQLite
history; it reads harness credential homes and never depended on the proxy)
and the **daemon supervision substrate** (pane daemon, agent supervision).

THE-58 asks for the resurrection framed as: per-mode/cost-aware model routing
(auto-pick tier by task) and fallback routing across runtimes (auto-failover
when a provider is down). The linked projects (llmtrim, rtk) are
token-compression tools — cost-adjacent context, not scope: both compose
externally with a local proxy endpoint.

## Goals / Non-Goals

**Goals**

- A local endpoint any configured agent CLI can point at, opt-in per agent.
- Tier routing (fast/standard/heavy — user-named), model aliasing, and a
  deterministic `auto` tier classifier; cost-aware lane ordering.
- Ordered failover across providers _and runtimes_ (local openai-compat
  servers — Ollama/vLLM — are just lanes), with health cooldown and failback.
- Per-request cost accounting attributable to worktree/workspace/agent, and
  per-scope budgets, surfaced through the existing usage/alert plumbing.
- Capability rows + doctor probe + SecretRef-only key custody.

**Non-goals** — ACP/bouncer/tool interception, the sealed-container bridge,
token compression (W), remote-sandbox tunnels, a TUI dashboard overlay, the
`thegn-agent` dialer. See proposal Non-goals.

## Decisions

### D1. Crate layout: revive `thegn-proxy`; pure logic in `thegn-core::proxy`

The old split is exactly the house pattern and is kept: `thegn-core::proxy` is
substrate-free (no tokio/HTTP) and carries every decision that can be unit
tested — classification, ordering, cost math, rate-bucket math, transforms,
stats, and the two new modules (tier classifier, usage-aware ordering) — under
the 95% gate. `thegn-proxy` is the tokio/axum shell (bind, relay, SSE) plus
the `ModelUpstream` seam impls' HTTP dispatch. `thegn-host` only supervises
the process and implements the CLI/status verbs. `core::backoff` (which
survived the excision because the placement spillover engine uses it) is
reused for restart pacing — not duplicated.

_Alternative considered_: fold the proxy into `thegn-host` as a mode of the
pane daemon. Rejected — it would couple an optional AI subsystem into the
default-on daemon binary path, grow the daemon's blast radius (it would then
hold provider keys), and contradict "the shell never hard-depends on it". A
separate process that only exists when `[model_proxy].enabled` keeps the
additive guarantee physical.

### D2. Config: a fresh `[model_proxy]` section, TOML-native registry

- **Not `[llm_proxy]`**: stale `[llm_proxy]` sections in pre-alpha user
  configs are tolerated-and-dropped with a `config_warn` today. Reusing the
  name would silently re-arm configs users were told are dead — enabling a
  proxy (and key resolution) without a deliberate opt-in. A new name makes
  resurrection an explicit act. The existing stale-section warning behavior is
  unchanged.
- **Not `[proxy]`**: reads like a network/HTTP proxy knob; the subsystem is a
  model proxy and the section name should say so.
- **TOML-native registry instead of the old side-JSON routes doc**
  (`config_path` → `TGPROXY_CONFIG`): providers and routes become
  `[[model_proxy.providers]]` / `[[model_proxy.routes]]` arrays in layered
  thegn config — schema'd (`schemars`), validated by `thegn config validate`,
  trust-clamped like every key, printed by `thegn config show`, documented in
  the generated config-reference help page. This also lands the archived
  `add-proxy-provider-registry` consolidation (one ProviderInfo-style table
  drives both the config surface and client construction). The daemon still
  receives its resolved config from the host at launch (serialized over env or
  a temp file with keys **excluded** — see D6; the proxy process resolves
  SecretRefs itself).

Sketch (documented in `config/config.toml.example` by the implementation):

```toml
[model_proxy]
enabled = false                  # opt-in; nothing runs when false
listen = "127.0.0.1:8383"        # loopback default; non-loopback warns
routing = "sequential"           # sequential | load_balanced | cost_aware
usage_aware = false              # factor [usage] account headroom into ordering
first_byte_timeout_secs = 45
idle_timeout_secs = 120
heartbeat_secs = 10

[[model_proxy.providers]]
name = "anthropic"
kind = "anthropic"               # anthropic | openai — else reserved
base_url = "https://api.anthropic.com"
api_key = "env:ANTHROPIC_API_KEY"   # SecretRef only (env:/file:)
# api_keys = ["env:KEY_A", "env:KEY_B"]  # multi-key lanes
# key_strategy = "roundrobin"    # roundrobin | failover | random | weighted
# rpm / burst / inflight_cap / context_limit / defaults / input_usd_per_mtok /
# output_usd_per_mtok / subscription = true (flat-rate lane, logs $0)

[[model_proxy.routes]]
name = "standard"                # tier name; client model id "model-proxy/standard"
backends = [ { provider = "anthropic", model = "claude-sonnet-4-5" },
             { provider = "ollama",    model = "qwen3:32b" } ]
# last_resort = false

[model_proxy.aliases]            # extra client model ids → routes
"gpt-5" = "standard"
"auto" = "@auto"                 # reserved: deterministic tier classifier

[model_proxy.budget]             # optional; absent = accounting only
# scope ceilings in USD, rolling windows; on_breach = warn | refuse | downgrade
```

### D3. Routing: explicit tiers first, `auto` as a pure classifier

Per-mode routing is layered so the deterministic mechanisms dominate:

1. **Exact alias / route name** — the client's requested model id maps
   directly (`model-proxy/fast`, alias table). This is how agent CLIs are
   configured per mode today and remains the primary mechanism.
2. **`auto` alias** — a pure `thegn_core::proxy::route_select` function picks
   a tier from request features only: estimated prompt tokens (the existing
   `transform::estimated_request_tokens`), tool presence, streaming flag, and
   an optional client hint header (`x-thegn-mode`). Deterministic, fully
   unit-tested; thresholds configurable per route (`auto_max_tokens` style
   bounds on the route entry). No model call, no learned heuristic.
3. **`cost_aware` strategy** (successor of `speculative`) — orders a route's
   lanes cheapest-first by the price table; combined with classification,
   failures fall through to costlier lanes.
4. **`usage_aware` ordering (new)** — an opt-in input from the `[usage]`
   tracker: lanes whose provider account sits past the `[usage.alerts]` warn
   threshold are deprioritized; past crit they are skipped — but never all:
   at least one lane must always stay eligible (a fully-throttled route
   degrades to plain ordering rather than refusing). Pure function over an
   `AccountUsage` snapshot passed in by the shell; the proxy never fetches
   quota itself.

Failover semantics are restored as-was: `Serve`/`Soft`/`Exhausted`
classification, per-identity cooldown with `Retry-After`/reset parsing,
half-open recovery, streaming first-byte peek fall-through, optional
cross-route `last_resort`.

### D4. Upstream seam (`thegn_svc`-style, in the proxy crate)

Provider dispatch is a seam per the house pattern, keyed by wire protocol —
not by vendor:

- `trait ModelUpstream` — object-safe, `BoxFuture` methods (`dispatch`,
  `dispatch_stream`), caps bits (streaming, tool passthrough, cache-token
  reporting), a `Probe` describing itself for `thegn doctor`.
- Two implementations cover the ecosystem: `AnthropicUpstream`
  (`/v1/messages`) and `OpenAiCompatUpstream` (`/v1/chat/completions` —
  OpenAI, OpenRouter, DeepSeek, Mistral, Groq, **Ollama/vLLM local
  runtimes**). Vendor quirks (header shapes, default bumps) live only inside
  the impl files.
- Config `kind` is implemented (`anthropic`, `openai`) or **reserved** (e.g.
  `gemini`, `bedrock` may be declared reserved so configs referencing them
  produce the standard implemented-or-reserved diagnostic instead of a parse
  error).

### D5. Accounting, DB, and `[usage]` integration

- **Fresh table names.** Old `proxy_*` tables exist orphaned in user DBs and
  are recreated by older builds via `CREATE TABLE IF NOT EXISTS` — reusing
  the names against a new schema would collide with old-shaped orphans.
  New tables: `model_proxy_requests` (per-request metadata: timestamps,
  route, backend, model, protocol, caller scope — workspace/worktree/agent —
  input/output/**cache-read/cache-create** tokens, cost USD + cost source,
  duration, TTFB, status, fail kind) and `model_proxy_budget_state` (window
  anchors/accumulators). Additive migration, `user_version` bump. Orphaned
  legacy tables are never dropped, read, or migrated (shared-DB contract).
- **Metadata only**: no prompt, message, tool-call, or response content is
  ever written to the DB or logs (D6).
- **Cache-token fields** are first-class (the fleet-view change's one proxy
  ask), parsed from Anthropic/OpenAI usage blocks when present.
- **Stats** (`thegn_core::proxy::stats`) restore the single rollup consumed
  by `/stats`, `thegn proxy stats`, and the usage panel — one math, three
  surfaces.
- **`[usage]` panel integration**: the System ▸ Usage section and `Alt u`
  usage dashboard gain a proxy-spend block (today/7d cost + tokens by route)
  when the proxy is enabled — hydrated off-thread from the DB on the existing
  usage refresh cadence, delivered over the usual channel + `TerminalWaker`
  pulse; chrome change ⇒ `Full` damage via the normal panel path. Help
  context stays `panel:usage` (`docs/help/ai-usage.md` gains the prose; no
  new actions/keybinds/zones/sections, so no help-ratchet entries).
- **Budget breaches** raise through the existing usage-alert/notification
  path (warn default). `refuse` / `downgrade` enforcement is restored from
  the old `budget.rs` in a later phase of this change's tasks; downgrade
  reuses tier ordering (drop to the cheapest eligible lane).

### D6. Security

- **Key custody**: `api_key` accepts **SecretRefs only** (`env:VAR`,
  `file:PATH`); a raw literal fails `thegn config validate` and refuses proxy
  start with a pointed diagnostic. Keys are resolved inside the `tgproxy`
  process at startup, held in memory only, never persisted to DB/state/logs,
  and redacted from `Debug` impls (restored `Backend` Debug already omits the
  key). The host passes the resolved config to the daemon **without** key
  material — the refs travel, the proxy resolves them.
- **No content logging**: request/response bodies, prompts, and tool payloads
  are never logged or persisted, at any log level, by default. There is no
  config knob for it; a debug build-only env (`THEGN_PROXY_UNSAFE_LOG_BODIES`)
  may exist for development and MUST print a loud startup warning — release
  behavior is unconditional.
- **Bind surface**: loopback default. A non-loopback `listen` is honored but
  drawn as a warning in `thegn doctor` and at startup (the endpoint holds no
  auth of its own beyond virtual keys; exposing it exposes metered spend).
- **Virtual attribution keys**: per-worktree tokens minted by the host and
  injected with `route_via_proxy`; they carry caller scope for accounting and
  are _not_ upstream secrets — leaking one leaks attribution + local spend
  capacity, never a provider key. They are random, revocable (regenerated per
  session), and checked only by the local proxy.
- **Injection safety**: `route_via_proxy` env injection
  (`ANTHROPIC_BASE_URL` / `OPENAI_BASE_URL` + virtual key) happens only after
  a successful liveness probe of `listen` at spawn time; probe failure skips
  injection and surfaces a status warning, so a down proxy can never strand
  an agent with a dead loopback endpoint (the documented `route_claude`
  hazard from the old design).
- **Blast radius**: the proxy process is the one place all provider keys
  coexist in memory. It runs as the user, binds loopback, and is sandboxed no
  differently than the host today; containing it further (systemd hardening,
  `thegn.slice` membership like other background jobs) is noted as an open
  question. New write surfaces: the two new DB tables (host + proxy write),
  and the proxy's listen socket (localhost-only by default).
- **Scopes**: `model_proxy.status`/`stats` require `read`;
  `model_proxy.start`/`stop` require `admin`. All four are OPERATOR-surface
  rows (HTTP/gRPC/CLI) — never MCP or plugin, so an MCP client or plugin
  cannot start a spend-capable daemon or read spend data through those doors.

### D7. Lifecycle and event-loop discipline

- The host launches `tgproxy` when `enabled`, off the UI loop (spawned from
  the existing background-work path, never before first frame). The **listen
  socket is the lock** — mirroring the pane daemon's "the IPC endpoint is the
  lock": a second bind loses and exits 0. Crash restarts pace with
  `core::backoff`; `thegn proxy stop` is graceful (SIGTERM → in-flight
  streams drain up to idle timeout).
- Event-loop touch points are exactly two, both existing patterns: the
  usage-panel spend hydration (off-thread → channel → waker → `Full`) and
  status-line warnings on supervision events (same path). The render decision
  function gains no new inputs; an idle proxy adds zero wakes. No new
  interactive surface, so no new help context key.
- `THEGN_E2E` freeze: the proxy is disabled under the e2e determinism freeze
  (like `[usage]`), so snapshots don't flap.

## Risks / Trade-offs

- **Scope weight**: this is a large resurrection. Mitigated by restoring from
  history (the router/relay/classify/cost code existed and was tested — old
  `tests/e2e.rs` was 1,508 lines and comes back with it) and by phasing:
  routing+failover first, budgets/enforcement last.
- **Bit-rot in restored code**: five months of main drift (store traits,
  config layering, control-plane changes). The tasks restore module-by-module
  behind compiling seams rather than one mega-revert.
- **Config expressiveness**: TOML-native routes lose the old JSON doc's
  copy-paste portability. Accepted — layered/validated/trust-clamped config
  wins; `thegn config show` covers export.
- **Two background daemons** (pane daemon + tgproxy). Accepted: the proxy is
  opt-in and per-state-dir; folding it into the pane daemon couples the
  AI-free shell to AI code (D1).
- **`add-fleet-view` drift**: it references the old table name and the
  excised proxy. This change re-provides the data; fleet-view re-targets when
  picked up. No edit to that change here.

## Migration Plan

Purely additive. No existing config key changes meaning; stale `[llm_proxy]`
sections keep their current warn-and-drop behavior. DB migration adds tables
and bumps `user_version`; older builds ignore the new tables (same contract
that protected the orphaned `proxy_*` set). Disabling `[model_proxy]` returns
the system to today's behavior with no residue beyond inert tables.

## Open Questions

- **Sandbox containment of `tgproxy`**: should the proxy join `thegn.slice`
  via `wrap_background_argv` like the fold gate and agent handoff (resource
  ceiling), or stay unwrapped since it is latency-sensitive I/O, not compute?
  Leaning wrapped-but-measured.
- **Budget enforcement default**: `on_breach = "warn"` is the proposed
  default; whether `downgrade` should ever be default-on for the `auto` alias
  is deferred until real spend data exists.
- **Tool-gating seam**: if the agent-harness unit (G6a) resurrects an
  in-process harness that needs tool interception, the sealed-egress bridge
  (`proxy/bridge.rs` in history) is _its_ dependency to claim — this change
  deliberately leaves it dead and its absence must not block proxy routing.
- **Prompt-cache preservation** (U 284): the transforms restored here keep
  bodies byte-stable where possible, but a cache-hit-ratio metric (V 297) and
  cache-aware lane stickiness are follow-ups.
