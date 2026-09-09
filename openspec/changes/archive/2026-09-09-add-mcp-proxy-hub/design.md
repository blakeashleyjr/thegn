# Design — MCP proxy hub

## Shape: two MCP servers, one hub

thegn ends up running two kinds of MCP endpoint, and they stay distinct:

- `thegn mcp serve` — thegn's **own** tools (docs, state; catalog-governed).
  Unchanged here.
- `thegn mcp proxy` — the **hub**: third-party upstreams aggregated behind
  one stdio endpoint. Upstream tools are _not_ thegn capabilities and are
  never minted as catalog rows; the catalog governs the hub's own control
  operations (`mcp_proxy.status`, `mcp_proxy.reload`) only. The filter policy
  (below) is the policy plane for the third-party surface, exactly as
  `required_scope(verb)` is for thegn's own.

An agent therefore registers at most two thegn MCP servers, both stable argv
(`thegn mcp serve`, `thegn mcp proxy`), regardless of how many upstreams exist.

## Where things run

```
agent CLI ──stdio──▶ thegn mcp proxy (shim, per agent)
                        │ control IPC (unix socket / named pipe)
                        ▼
                  thegn daemon ── upstream supervisor
                        ├─ upstream A (stdio child, shared, thegn.slice)
                        ├─ upstream B (per-workspace instance)
                        └─ health ticker → breaker transitions
```

- **Daemon mode (default when `[daemon] enabled`):** the daemon owns one
  upstream process per (server, partition key). Shims multiplex over the
  existing control IPC; JSON-RPC ids are rewritten per-shim so concurrent
  agents interleave safely. Upstreams survive UI detach like PTY sessions do.
  All timers (health, reconcile mtime check) are daemon-process tokio tasks —
  the compositor's 0%-idle contract binds the UI loop, which this change
  never touches; there is no new UI tick, no render-plan involvement.
- **Standalone fallback (no daemon):** the shim spawns its own upstream
  children and runs the same core aggregation in-process. Sharing degrades
  (per-agent instances); behavior does not. The wrap is fail-safe in the
  house style: an unreachable daemon degrades to standalone, and a broken
  upstream degrades to "that upstream's tools absent" — never a dead
  endpoint.
- **Spawn path:** upstream argv passes through `wrap_background_argv` (the
  `thegn.slice` ceiling) and env-ref resolution; vendor-specific agent
  settings paths live only in `wire`'s per-agent adapter files (seam rule).

## Core seams (`thegn-core`, pure, 95% gate)

- `mcp::proxy::aggregate` — merge upstream `tools/list` replies under
  namespaced names (`<upstream>__<tool>`; both segments already match the MCP
  name charset), apply the filter, produce the advertised tool table. Pure
  over `serde_json::Value`s.
- `mcp::proxy::route` — `tools/call` name → (upstream, original tool);
  unknown/filtered names get a JSON-RPC error, never a passthrough.
- `mcp::proxy::filter` — default-deny evaluation of
  `[mcp_servers.<name>.proxy] tools` globs (reuses the grants glob matcher
  semantics: `*` within a segment).
- `mcp::proxy::breaker` — the circuit state machine: `Closed` →(N consecutive
  failures/timeouts)→ `Open` →(cooldown)→ `HalfOpen` →(probe ok)→ `Closed`.
  Pure transitions over injected clocks; table-tested.
- `mcp::proxy::reconcile` — old effective config × new → a plan of
  start/stop/restart/refilter per upstream instance (hot reload is applying
  this plan; the diff is exhaustively unit-testable).
- `mcp::proxy::partition` — placeholder expansion (`{workspace}`,
  `{worktree}`, `{repo_root}`, `{branch}`) + the instance key derivation for
  `scope = global|workspace|worktree`.
- `secret::SecretStore` — seam vocabulary for the keyring: object-safe trait
  (`get`, `set`, `del`, `list` under a thegn service namespace), `Probe` for
  doctor, `kind` implemented-or-`reserved`. Impls (Secret Service / Keychain
  / Windows Credential Manager) live behind the seam in `thegn-svc`/host
  platform code; unsupported platforms probe as unavailable and `keyring:`
  refs there resolve to a clear error, not a hang.

Host-side I/O (child processes, IPC pump, keyring FFI) is smoke-tested; the
decision logic above is where the coverage lives.

## Partitioning (the THE-49 mechanism)

A shim knows its worktree identity from its cwd (agents launch it from inside
their pane; resolution uses the same DB/daemon lookup other cwd-anchored verbs
use). On connect it presents that context; the daemon selects or spawns the
upstream instance for the derived partition key and expands placeholders into
that instance's env/args. Outside any known worktree, `workspace`/`worktree`
-scoped upstreams are withheld from that connection (with a `status`-visible
reason) rather than silently sharing a global namespace — partition leakage is
treated as a correctness bug, not a degradation.

## Memory: preset, not feature (THE-49 decision)

Evaluated at README level: mem0 (Apache-2, 64k★, SDK/self-host/cloud, needs
LLM+embedding API), cognee (Apache-2, 30k★, graph+vector, MCP via Docker,
needs LLM), supermemory (MIT, 29k★, hosted MCP + `npx … local`), agentmemory
(Apache-2, 27k★, local-first SQLite, MCP + hooks), beads (MIT, 26k★, Go,
issue-graph-as-memory, CLI+MCP), memex (Rust, transcript search, skill-based
not MCP), SeekStorm (Rust lexical search — a substrate, not a memory server).

The decision is **thegn does not build or bundle a memory engine**:

1. Every viable option above already speaks MCP or skills — the surfaces thegn
   already brokers. Integration is configuration, not code.
2. The field turns over monthly on benchmarks (LongMemEval/LoCoMo); a
   first-party engine would be a worse mem0 with a maintenance tail.
3. Memory is agent-only; the post-excision shell must not grow agent-only
   subsystems (CLAUDE.md "What this is"). A preset is inert until declared —
   optional by construction.
4. "Seams, not vendors": thegn's durable contribution is what no vendor can
   do from outside — per-workspace/worktree partitioning, credential custody,
   lifecycle/health, and default-deny filtering. All of that is the proxy.

What ships: curated presets (`thegn mcp preset`) as embedded data — each a
vetted `[mcp_servers]` block with source pin, least-privilege grants, a
default partition `scope`, and a note on external requirements (API keys,
Docker). At least one preset MUST be fully local with no API key so memory
works offline. Skill-shaped options (memex, beads' CLI habits) belong to
`add-embedded-skills`' curated set, not here. Presets print by default;
`--write` appends to the user config after showing the block — presets never
silently edit config.

## Hot reload

`thegn mcp reload` (write scope) re-reads config and applies the reconcile
plan. The daemon also stats the config file on its existing heartbeat and
reconciles on mtime change — no new timer, no UI involvement. Either path ends
by emitting `notifications/tools/list_changed` to connected shims, so agents
that honor the MCP notification refresh their tool tables without restart.
Breaker trips do _not_ change the advertised table (tools go
temporarily-erroring, not vanishing) — flapping advertisements confuse agents
more than a clean error does.

## Alternatives considered

- **Retrieval-first meta-tools** (a `search_tools`/`execute` pair instead of
  advertising every tool — MCProxy/"proxy pattern" style): better token
  economy at large tool counts, but breaks agents' native tool UX and
  approval flows. Deferred; the namespaced-advertisement design does not
  preclude adding a meta-tool mode later.
- **Proxy inside `thegn mcp serve`:** one endpoint fewer, but it would blur
  the catalog boundary (thegn tools are catalog-governed; upstream tools are
  filter-governed) and force `serve`'s docs-only default to carry proxy
  policy. Rejected.
- **Per-call header/argument injection for partitioning** (e.g. supermemory
  "project" tags): vendor-specific and silently lossy. Rejected for
  instance-per-scope env templating, which is generic.
- **Allow-by-default with a blacklist** (TUICommander offers both): rejected;
  aggregation multiplies exposure, and the security posture here is
  default-deny (below).
- **A `[mcp_proxy] upstreams = [...]` list instead of per-server subtables:**
  splits one server's declaration across two tables; rejected — exposure is a
  property of the server declaration.

## Security

- **Tool filtering is default-deny.** An upstream exposes nothing until its
  `proxy.tools` list exists; `["*"]` must be typed deliberately. Rationale:
  upstream tool names/descriptions are untrusted input (tool-poisoning /
  prompt-injection carriers) and aggregation multiplies the blast radius of
  one malicious upstream across every wired agent. `thegn mcp list` and
  doctor show exposed-vs-hidden per upstream so the policy is inspectable.
- **Credential custody.** Raw tokens never belong in config (house rule);
  upstream env supports `env:`/`file:`/`keyring:` refs resolved only at
  upstream spawn, inside the daemon (or standalone shim) process. The
  emitted/wired agent entry contains argv only — no env, no secrets — so
  agent settings files stop being a credential store (today's `mcp emit`
  copies `env` verbatim into them; the proxy path removes that exposure).
  Keyring entries live under a namespaced service id; `mcp secret list`
  names entries, never values. Secrets are redacted from `status`, logs, and
  doctor output.
- **Upstream isolation.** Upstreams are child processes joined to
  `thegn.slice` (resource ceilings), spawned with only their declared env —
  not the daemon's, beyond a minimal base — and no IPC access to the daemon
  besides their own stdio. A compromised upstream can lie in its JSON-RPC
  replies but cannot reach other upstreams' stdio, the keyring, or the
  control socket through thegn. Launch specs are user config — the
  config-trust verdicts from `add-config-trust-resolution` gate them like any
  other config-declared command.
- **Write surfaces.** `wire` edits agent settings files: explicit command
  only, marker-tagged entries, idempotent merge, `--remove` reversal, and a
  refusal to touch entries it did not mark. `preset --write` appends to thegn
  config after printing. `reload` is write-scoped (a read-scoped client must
  not be able to flip filter policy application). Neither `wire` nor
  `preset` is exposed on MCP/plugin surfaces.
- **No new listening sockets.** stdio + the existing control IPC only. Remote
  transports (and their OAuth 2.1 story) are deliberately `reserved` until
  they can be designed with the same custody rules.

## Open questions

- Does the daemon already carry a config-watch we should reuse instead of the
  heartbeat mtime check? (Implementation detail; the spec only requires
  "reload on command, and automatically within a bounded delay".)
- Should `keyring:` refs generalize to every secret-ref site in config
  (forge tokens, tunnel secrets)? The seam is built for it; this change only
  specs MCP upstream env to keep the blast radius reviewed. Follow-up spec if
  wanted.
- MCP-surface exposure of `mcp_proxy.status` via `thegn mcp serve` state
  tools — desirable, but sequenced behind the in-flight write-MCP-tools
  branch; the delta lists Cli/Http/Grpc only.
- Preset contents (which vendors, which pins) are implementation-time
  curation; the spec fixes the _shape_ (pin + grants + scope + local-first
  guarantee), not the vendor list.
