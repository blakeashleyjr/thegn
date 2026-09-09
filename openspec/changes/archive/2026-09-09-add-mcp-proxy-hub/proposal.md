# Add MCP proxy hub (one endpoint per agent) + curated memory presets

Linear: THE-16
Linear: THE-49

## Why

Today every agent consumes `[mcp_servers.<name>]` directly: `thegn mcp emit`
copies each server's launch spec — including its `env`, i.e. its credentials —
into every agent CLI's settings file. N agents × M servers means N×M server
processes, N plaintext copies of every upstream secret, no health supervision,
no lifecycle, and no way to hide a dangerous tool from one agent. The
competition (TUICommander's "MCP Proxy Hub", MetaMCP, MCPJungle, MCProxy) has
already converged on the fix: aggregate all upstreams behind **one MCP
endpoint per agent**, with the hub owning lifecycle, health, credentials, and
tool filtering.

The old roadmap wanted exactly this (AR 541 central MCP registry, 542
lifecycle, 543 credential brokerage, 546 tool filtering) but hung it off the
excised LLM proxy. An MCP hub needs none of that: it is JSON-RPC tool
plumbing, not model traffic — a strictly generic, AI-free-shell-compatible
surface, like the `[[agents]]` picker and the queues' agent hook.

THE-49 (a thegn-wide memory system for agents) is resolved here as a design
decision rather than a feature: every credible memory system (mem0, cognee,
supermemory, agentmemory, beads, memex) already ships as an MCP server or a
skill, the field is fast-moving and benchmark-driven, and a memory engine is
agent-only — precisely what the post-excision shell must not grow. **Memory is
not a thegn feature; it is a curated `[mcp_servers]` preset riding the proxy**,
with thegn contributing the two things only it can: per-workspace/worktree
**partitioning** and credential custody. See `design.md` for the full argument.

## What Changes

- **New capability `mcp-proxy`** — `thegn mcp proxy`, a stdio MCP endpoint an
  agent registers as its single MCP server. It aggregates the tools of every
  _exposed_ `[mcp_servers.<name>]` upstream (namespaced
  `<upstream>__<tool>`), routes `tools/call` to the owning upstream, and
  merges `tools/list`. Pure aggregation/routing/filter logic lives in
  `thegn_core::mcp::proxy` (95% gate); process I/O in `thegn-host`.
- **Daemon-hosted upstreams, shared across agents.** When the pane daemon is
  enabled, upstream server processes are owned by the daemon — one instance
  per upstream (per partition scope), shared by every connected agent,
  surviving UI detach. `thegn mcp proxy` is then a thin shim over the
  existing control IPC. With the daemon disabled, the shim runs upstreams
  in-process (same core logic, degraded sharing) — fail-safe, never bricking
  an agent.
- **Default-deny tool filtering.** An upstream contributes nothing to the
  proxy until `[mcp_servers.<name>.proxy]` declares `tools = [...]` (globs;
  `["*"]` is the deliberate everything opt-in). Unlisted tools are never
  advertised or callable through the proxy.
- **Health checks + circuit breakers + hot reload.** The daemon health-checks
  upstreams on an interval; a pure breaker state machine
  (closed → open → half-open) fences a failing upstream without taking down
  the endpoint; config changes reconcile upstreams via a pure diff plan
  (`thegn mcp reload`, plus mtime-checked reconcile on the daemon's existing
  heartbeat) and emit `notifications/tools/list_changed` to connected agents.
- **Credential custody.** Upstream `env` secret refs (`env:` / `file:` / new
  `keyring:` via an OS-keyring `SecretStore` seam) resolve inside the daemon
  at upstream spawn. The agent-side proxy entry emitted by `wire`/`emit`
  carries **no env** — agents get the tools, never the keys (AR 543's payoff,
  AI-free). `thegn mcp secret set|rm|list` manages keyring entries.
- **Agent auto-configuration.** `thegn mcp wire [--agent <kind>] [--remove]`
  writes/merges the single proxy entry into supported agent CLIs' MCP
  settings (claude, codex, cursor, windsurf, vscode, zed, amp, gemini —
  per-vendor adapters inside impl files), marker-tagged, idempotent, and
  reversible. `[[agents]]` remains the source of truth for which agents.
- **Partitioning (THE-49).** `[mcp_servers.<name>.proxy] scope =
"global"|"workspace"|"worktree"` gives an upstream one instance per scope
  key, with `{workspace}`/`{worktree}`/`{repo_root}`/`{branch}` placeholders
  expanded in its env/args — generic per-project memory namespaces with no
  vendor knowledge in thegn.
- **Curated presets (THE-49).** `thegn mcp preset list|show <name> [--write]`
  ships vetted `[mcp_servers]` blocks (memory servers among them; at least one
  fully local, no-API-key option) as data — evaluated prior art, not adopted
  dependencies.
- **Catalog + doctor.** Two new capability rows (`mcp_proxy.status` read,
  `mcp_proxy.reload` write) projected per the catalog contract; `thegn
doctor` probes each exposed upstream (handshake, tool count, breaker
  state, filter summary) and the keyring backend.
- **Resource ceilings.** Daemon-spawned upstreams join the shared
  `thegn.slice` via the existing `wrap_background_argv`, so a greedy upstream
  is bounded by `[sandbox.limits]` like every other thegn-started process.

## Impact

- **Specs:** new `mcp-proxy` capability (ADDED); `mcp-servers` (ADDED:
  proxy exposure/filter/scope keys, placeholder templating, presets);
  `capability-catalog` (ADDED: the two proxy control rows).
- **Roadmap:** delivers the AI-free core of **AR 541/542/543/546** (registry,
  lifecycle, credential brokerage, tool filtering — re-grounded off the
  excised proxy) and answers **AR 550/566–569** (memory) as
  presets-not-features; sibling of **AL 455** (`thegn mcp serve` stays
  thegn's _own_ tool endpoint; the proxy is the _third-party_ aggregation
  endpoint — the two remain separate servers).
- **Config:** new `[mcp_proxy]` table + `[mcp_servers.<name>.proxy]` subtable
  - `keyring:` secret-ref kind — every key documented in
    `config/config.toml.example`.
- **Code (planned):** `thegn-core/src/mcp/proxy/` (pure aggregation, filter,
  breaker, reconcile-diff, placeholder expansion), `thegn-core` `SecretStore`
  seam vocabulary, `thegn-svc` keyring impls (per-OS, behind the seam),
  `thegn-host/src/cmd/mcp.rs` growth in sibling modules
  (`cmd/mcp_proxy.rs`, `cmd/mcp_wire.rs`), daemon upstream supervisor module.
- **DB:** none — breaker/health state is in-memory daemon state; config is
  the source of truth. No `user_version` bump.
- **In-flight reconciliation:**
  - _Write MCP tools with scope gating_ (in progress on a branch:
    parameterised state tools, `--scopes`, SURFACE_GAPS retirement) — this
    change's catalog rows and any MCP-surface exposure of `mcp_proxy.status`
    land **after** that work; treat it as a dependency, do not re-scope it.
  - `add-skills-registry` — its registry/injection rides the excised
    LLM proxy; nothing here builds on it. The sibling `add-embedded-skills`
    change covers the AI-free skills story.
  - `add-config-trust-resolution` — the trust model for config-declared
    commands applies to upstream launch specs; the proxy consumes whatever
    trust verdicts that change establishes rather than inventing its own.
  - `make-daemon-default` — strengthens the shared-upstream default; the
    standalone fallback keeps the proxy working either way.
- **Non-goals:** HTTP/SSE/streamable-HTTP upstream transports (the transport
  is a seam; remote transports and OAuth 2.1 are `reserved` and ride a
  follow-up), tool _description_ overriding (AR 546's other half),
  cross-host proxying, a TUI panel for proxy state (doctor + CLI only in
  v1), building or bundling any memory engine, and auto-wiring agent
  settings without an explicit user command.
