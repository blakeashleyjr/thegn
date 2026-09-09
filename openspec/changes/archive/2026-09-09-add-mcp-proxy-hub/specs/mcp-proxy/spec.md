# MCP Proxy

## ADDED Requirements

### Requirement: One aggregated MCP endpoint per agent

thegn SHALL provide `thegn mcp proxy`, a stdio MCP endpoint that aggregates
every exposed `[mcp_servers.<name>]` upstream behind a single server: it MUST
merge upstream tool lists under namespaced names (`<upstream>__<tool>`), route
each `tools/call` to the owning upstream, and answer `initialize` with merged
capabilities — so an agent registers one thegn entry instead of one entry per
upstream. Aggregation, routing, and namespacing MUST be pure `thegn-core`
logic, unit-tested without child processes.

#### Scenario: Two upstreams, one tool table

- **WHEN** two exposed upstreams each advertise a tool named `search` and an
  agent connected to `thegn mcp proxy` requests `tools/list`
- **THEN** the reply contains `a__search` and `b__search`, each carrying its
  upstream's schema, and calling `a__search` dispatches to upstream `a`'s
  `search`

#### Scenario: Unknown tool is an error, not a passthrough

- **WHEN** an agent calls a tool name that no exposed upstream advertises
- **THEN** the proxy answers a JSON-RPC error and sends nothing to any
  upstream

### Requirement: Tool exposure is default-deny per upstream

An upstream SHALL contribute nothing to the proxy until its
`[mcp_servers.<name>.proxy]` table declares a `tools` list (glob patterns;
`["*"]` is the explicit everything opt-in). Tools not matched by the list MUST
be neither advertised nor callable through the proxy, and `thegn mcp list`
MUST show each upstream's exposed-vs-hidden tool state so the effective policy
is inspectable.

#### Scenario: Undeclared upstream is invisible

- **WHEN** a `[mcp_servers.<name>]` has no `proxy.tools` list and the proxy
  serves an agent
- **THEN** none of that upstream's tools appear in `tools/list` and calls to
  them fail, even though the upstream remains usable via direct (non-proxy)
  agent configuration

#### Scenario: Filtered tool is unreachable

- **WHEN** an upstream's `proxy.tools = ["read_*"]` and the upstream also
  advertises `delete_page`
- **THEN** `delete_page` is absent from the aggregated list and a direct call
  to `<upstream>__delete_page` is refused by the proxy

### Requirement: Upstreams are supervised, shared, and resource-bounded

When the pane daemon is enabled, upstream server processes SHALL be owned by
the daemon — one instance per (upstream, partition key) shared across all
connected agents, surviving UI detach — and `thegn mcp proxy` SHALL act as a
per-agent shim over the existing control IPC with per-connection JSON-RPC id
rewriting. Upstream argv MUST pass through the shared background wrap
(`thegn.slice` resource ceilings), and daemon unavailability MUST degrade the
shim to running upstreams in-process rather than failing the endpoint. No part
of the proxy runs on the compositor event loop.

#### Scenario: Two agents share one upstream

- **WHEN** two agents' shims connect while the daemon is running and both use
  a global-scoped upstream
- **THEN** exactly one upstream process exists and both agents' calls are
  served by it, with responses correlated back to the caller

#### Scenario: No daemon still serves

- **WHEN** the daemon is disabled or unreachable and an agent launches
  `thegn mcp proxy`
- **THEN** the shim spawns its own upstream processes and serves the same
  aggregated endpoint

### Requirement: Failing upstreams are circuit-broken, not fatal

The proxy SHALL health-check upstreams on a configured interval and drive a
per-upstream-instance circuit breaker (closed → open on consecutive
failures/timeouts, open → half-open after a cooldown, half-open → closed on a
successful probe). While a breaker is open, calls to that upstream's tools
MUST fail fast with an error naming the upstream, other upstreams MUST be
unaffected, and the advertised tool table MUST NOT churn. Breaker transitions
MUST be a pure, clock-injected state machine in `thegn-core`.

#### Scenario: One dead upstream leaves the rest serving

- **WHEN** an upstream process wedges past its failure threshold
- **THEN** its breaker opens, its tools return fast errors naming it, and
  tools of other upstreams keep working

#### Scenario: Recovery closes the breaker

- **WHEN** an open breaker's cooldown elapses and the half-open probe succeeds
- **THEN** the breaker closes and calls flow to the upstream again

### Requirement: Configuration changes hot-reload

`thegn mcp reload` SHALL re-read config and reconcile running upstreams via a
pure diff plan (start added, stop removed, restart changed, refilter
in-place), and the daemon SHALL also reconcile automatically within a bounded
delay of a config-file change without any new compositor work. After a
reconcile that changes the advertised tool set, the proxy MUST emit
`notifications/tools/list_changed` to connected agents.

#### Scenario: Adding an upstream needs no agent restart

- **WHEN** a user adds an exposed upstream to config and triggers reload
- **THEN** the upstream starts, the tool table grows, and connected agents
  receive `notifications/tools/list_changed`

### Requirement: The proxy holds upstream credentials; agents never see them

Upstream `env` values SHALL support `env:`, `file:`, and `keyring:` secret
references, resolved only at upstream spawn inside the daemon (or standalone
shim) process. The agent-side proxy entry produced by `wire`/`emit` MUST
contain no environment block, and secrets MUST be redacted from `status`,
`list`, doctor, and log output. `keyring:` references SHALL resolve through an
OS-keyring `SecretStore` provider seam (object-safe trait, per-platform
implementations, `reserved` where unimplemented, Probe in `thegn doctor`),
managed by `thegn mcp secret set|rm|list` under a namespaced service id —
`list` naming entries, never values. On a platform whose keyring probe fails,
a `keyring:` reference MUST fail that upstream's spawn with a clear message
rather than hang or fall back to plaintext.

#### Scenario: Wired agent entry is secret-free

- **WHEN** an upstream declares `env = { API_KEY = "keyring:foo" }` and the
  user wires an agent
- **THEN** the agent's settings gain only the proxy argv — no `API_KEY`, no
  resolved value — and the upstream process receives the resolved value at
  spawn

#### Scenario: Missing keyring entry is a clean refusal

- **WHEN** a `keyring:` reference names an entry the store does not hold
- **THEN** that upstream fails to spawn with a message naming the entry and
  the `mcp secret set` remedy, and other upstreams are unaffected

### Requirement: Agent CLIs are wired by explicit, reversible command

`thegn mcp wire [--agent <kind>] [--remove]` SHALL write the single proxy
entry into supported agent CLIs' MCP settings via per-vendor adapters
(vendor-specific paths and formats confined to their implementation files).
Wiring MUST be explicit (never a side effect of launch), idempotent,
marker-tagged so thegn-managed entries are distinguishable, and MUST refuse to
modify or remove entries it did not mark. `--remove` MUST restore the settings
to their pre-wire state with respect to thegn's entries. The `[[agents]]` list
is the source of truth for which agents are wired by default.

#### Scenario: Wire twice, one entry

- **WHEN** `thegn mcp wire` runs twice for the same agent
- **THEN** the agent's settings contain exactly one thegn-marked proxy entry

#### Scenario: User entries are untouchable

- **WHEN** an agent's settings contain a user-authored MCP entry and
  `thegn mcp wire --remove` runs
- **THEN** only thegn-marked entries are removed

### Requirement: Upstreams partition by workspace or worktree

`[mcp_servers.<name>.proxy] scope = "global"|"workspace"|"worktree"` (default
`global`) SHALL give an upstream one instance per scope key, with
`{workspace}`, `{worktree}`, `{repo_root}`, and `{branch}` placeholders
expanded in that instance's env/args from the connecting shim's worktree
context (resolved from its cwd). A connection whose context cannot satisfy an
upstream's scope MUST have that upstream withheld with an inspectable reason
rather than being served a shared or misattributed instance.

#### Scenario: Two workspaces, two memory namespaces

- **WHEN** a workspace-scoped upstream templates `{workspace}` into its env
  and agents connect from worktrees of two different workspaces
- **THEN** two upstream instances run, each with its own expanded env, and
  each agent only reaches its workspace's instance

#### Scenario: Context-less connection withholds scoped upstreams

- **WHEN** a shim starts outside any registered worktree
- **THEN** workspace/worktree-scoped upstreams are absent from its tool table
  and `mcp_proxy.status` names the reason, while global upstreams serve
  normally

### Requirement: Proxy state is inspectable via status and doctor

`mcp_proxy.status` SHALL report, per upstream instance: running state,
partition key, breaker state, health-check recency, exposed tool count, and
withheld-connection reasons. `thegn doctor` SHALL probe each exposed upstream
(spawnability/handshake, tool count against the filter) and the keyring
backend, reporting per the provider-seam Probe contract.

#### Scenario: Doctor shows the policy and the health

- **WHEN** `thegn doctor` runs with exposed upstreams configured
- **THEN** each upstream is listed with its handshake result, exposed/hidden
  tool counts, and scope, and the keyring backend reports available or
  reserved
