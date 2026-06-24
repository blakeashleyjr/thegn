# ACP and the two-layer control plane

_Design / strategy note — 2026-06-24._

Companion to `2026-06-22-embedded-agent-integration-design.md`. That doc locked
the **embedded-first** harness strategy (termite-agent as the `agent` app tab).
This doc answers a different question: **how far have we pushed the
[Agent Client Protocol](https://agentclientprotocol.com) (ACP), and have we
pushed it to the max?** Short answer: **no, not yet** — and the reason is
instructive.

## TL;DR

1. We **independently re-invented ACP's frontier at a different layer**. ACP's
   own `proxy-chains` RFD ("Agent Extensions via ACP Proxies") describes almost
   exactly our **AI gateway / context fabric** vision (`tasks.md` group AR,
   541–586): inject prompts and context, filter/override tools, transform
   responses, coordinate across agents — _"configure once, every agent inherits
   it."_
2. The difference is **where the interception happens**:
   - **ACP proxies sit _above_ the harness** — in the client↔agent JSON-RPC
     stream (prompts, tool calls, session updates, permissions).
   - **Our `szproxy` (U/V/W) sits _below_ the harness** — in the agent↔model
     HTTP stream (Anthropic/OpenAI traffic).
   These are **two complementary layers of one control plane**, not competitors.
3. Our roadmap's ACP treatment (**group R**) is a stale 14-item stub written
   before half of today's ACP surface stabilized, and the embedded-first pivot
   _demoted_ it to "secondary/additive." It only ever models superzej as an ACP
   **client** — never as an ACP **agent** (a distribution play) or an ACP
   **proxy** (the upper-layer realization of AR).

The maximal move is to name the two planes explicitly, make ACP a co-primary
**upper** plane with three roles (client / agent / proxy), and wire it to
`szproxy` at two well-defined seams.

## The two-layer control plane

```
        ┌────────────────────── superzej (the Client / IDE) ──────────────────────┐
        │                                                                          │
 user ─►│  ACP CLIENT ──(UPPER plane: ACP proxy / AR — R3)──►  AGENT (harness)     │
        │      ▲                                                     │             │
        │      │                                                     │             │
        │      │  providers/set(baseUrl = szproxy)   MCP-over-ACP    ▼             │
        │   szproxy ◄───────(LOWER plane: U/V/W model traffic)─────  model API     │
        │                                                                          │
        └──────────────────────────────────────────────────────────────────────────┘
```

- **Lower plane — `szproxy` (U/V/W → AR).** _Already built._ Owns model traffic:
  dual-protocol relay, failover, per-scope budgets, spend attribution, in-flight
  token reduction. Agnostic to _which_ harness produced the request.
- **Upper plane — ACP (group R).** Owns the agent _conversation_: sessions, tool
  calls, permissions, diffs, plans, config options, slash commands, telemetry.
- **Two convergence seams join them:**
  - **`providers/set`** (Configurable LLM Providers RFD) — point _any_ ACP
    agent's model traffic at `szproxy` by setting `baseUrl` + brokered
    `headers`. This is the literal bridge between R and U.
  - **MCP-over-ACP** — advertise AR's central MCP registry / house tools _up_ to
    any agent over the ACP channel (`mcp/connect`/`message`/`disconnect`), with
    credentials brokered, no open ports.

This preserves the embedded-first investment (termite stays first-party) while
making ACP co-primary instead of an afterthought.

## ACP surface map (v1 stabilized)

| Area | Methods / fields |
| --- | --- |
| Init / capabilities | `initialize` (protocolVersion; `clientCapabilities`: `fs.readTextFile`/`fs.writeTextFile`/`terminal`, `clientInfo`); `agentCapabilities` (`loadSession`, `promptCapabilities`: image/audio/embeddedContext, `mcpCapabilities`: http/sse/acp); `authMethods`; `agentInfo`. All caps optional; omitted ⇒ unsupported |
| Auth | `authenticate`, `logout` (+ `agentCapabilities.auth.logout`) |
| Sessions | `session/new`, `session/load`, `session/resume` (reconnect, no replay), `session/list`, `session/close`, `session/delete`, `session/fork`, `session_info_update` |
| Prompt turn | `session/prompt` → `session/update` (`agent_message_chunk`, `agent_thought_chunk`, `tool_call`, `tool_call_update`, `plan`, `available_commands_update`, `usage_update`, `config_option_update`) → StopReason (`end_turn`/`max_tokens`/`max_turn_requests`/`refusal`/`cancelled`); `session/cancel` |
| Tool calls | status pending/in_progress/completed/failed; kinds read/edit/delete/move/search/execute/think/fetch/other; content regular / diff (`oldText`+`newText`+`path`) / terminal (`terminalId`); `locations` (path+line, follow-along); `rawInput`/`rawOutput` |
| Permissions | `session/request_permission` (allow_once/allow_always/reject_once/reject_always → optionId \| cancelled) |
| Filesystem | `fs/read_text_file`, `fs/write_text_file` (client-provided) |
| Terminals | `terminal/create` (command/args/env/cwd/outputByteLimit), `terminal/output`, `terminal/wait_for_exit`, `terminal/kill`, `terminal/release`; embeddable in tool calls |
| Config options | supersedes Session Modes; `configOptions` (id/name/category model\|mode\|thought_level/type select/currentValue/options), `session/set_config_option`, `config_option_update` |
| Content / transport / registry | content blocks text/image/audio/resource/resource_link; stdio JSON-RPC (HTTP/WebSocket WIP); ACP Registry (`registry.json` CDN + `agent.json` manifest + one-command install) |

## Frontier RFDs (where "the max" lives)

| RFD | What it adds | Our analog |
| --- | --- | --- |
| **Configurable LLM Providers** | `agentCapabilities.providers`; `providers/list`/`set`/`disable` (id/apiType/baseUrl/headers) | **The R↔U bridge** — point any ACP agent at `szproxy` |
| **Agent Extensions via ACP Proxies** (`proxy-chains`) | proxies in the message flow; `proxy/initialize` + `proxy/successor` via a conductor; subsumes AGENTS.md, hooks, plugins, MCP | **Upper-layer twin of AR** (541–551) |
| **MCP-over-ACP** | `{type:"acp"}` servers; `mcp/connect`/`message`/`disconnect`; `mcpCapabilities.acp` | AR central MCP registry (541–543) + AL exposed up to any agent |
| **Session Context Size & Cost** | `usage_update` (`used`/`size` tokens + optional `cost{amount,currency}`) | S 246/249/250 + V 289/290 spend attribution |
| **Agent Telemetry Export** | `OTEL_EXPORTER_OTLP_ENDPOINT` env injection + `params._meta` traceparent | S 254 OTEL ingestion; perf/observability suite |
| **Elicitation** | `elicitation/create` (form mode w/ restricted JSON Schema; URL mode for OAuth; accept/decline/cancel) | native iocraft palette/form UI; AL 459 |
| **ACP v2** | unified `capabilities` object; object-valued markers; session-scoped caps; item-based `plan_update`; upsert `tool_call`; content chunks; MCP transport alignment | build R v2-shaped from day one |

## Gap analysis — have we maxed it?

**What we already nailed (lower plane, U/V/W — shipped):** dual-protocol relay,
ordered failover + load-balanced/speculative routing, limit-exhaustion / reset
tracking, per-scope budgets, spend attribution, native token reduction, daemon
auto-launch, per-worktree scoped virtual keys. This is genuinely ahead of where
most ACP clients are — but it is the _model_ layer, not the _protocol_ layer.

**What we under-built (upper plane, group R):**

- Modeled superzej only as an ACP **client**; never as an **agent** (export
  termite to Zed/other editors via the Registry) or a **proxy** (AR for any
  agent).
- Group R's "session management" (item 230) is one line; ACP now has
  new/load/**resume**/**list**/**close**/**delete**/**fork** as distinct
  stabilized features that map cleanly onto our worktree-tabs + session
  resurrection + time-travel-replay model.
- We are a **terminal multiplexer** and have a **sandbox** — making us the
  natural _premier_ ACP terminal client (`terminal/*` through our PTY +
  `sandbox::enter_argv`) and filesystem client (`fs/*`). Group R doesn't mention
  either.
- `usage_update`, Configurable LLM Providers, MCP-over-ACP, Elicitation,
  Telemetry Export, Session Config Options, and the v2 redesign are entirely
  absent from R.

**Verdict:** we maxed the _lower_ plane and barely scoped the _upper_ one. The
roadmap rewrite (below) closes that gap.

## Roadmap changes (see `tasks.md`)

Group R is rewritten from a flat 14-item list into three role-based
sub-sections; new items land in the free **684+** band, original numbers
229–242/657 preserved.

- **R1 · ACP Client** — modernized client: full session lifecycle, full
  `session/update` set, config options, `usage_update`, elicitation, fs +
  terminal surfaces, **Configurable LLM Providers** (the R↔U bridge), telemetry
  export, v2 readiness.
- **R2 · ACP Agent** — termite-agent as an ACP _agent server_ (consumable by
  foreign editors; submit to the Registry); emit `plan`/`tool_call`/
  `usage_update` over ACP.
- **R3 · ACP Proxy** — realize the AR gateway as an **ACP proxy**
  (`proxy/initialize` + `proxy/successor` + conductor) so capability injection /
  prompt layering / tool filtering work with _any_ ACP agent; **MCP-over-ACP**
  exposure of the central MCP registry with brokered creds.
- **236–242** (native adapters) reframed as the explicit _fallback for non-ACP
  harnesses_, ACP-registry-first.

Cross-reference tags added in the `tasks.md` header, AR header, and groups
U/V/AL/S so the two-plane framing reads consistently.

## Implementation seam (for when R is built — not built here)

- **JSON-RPC transport**: `crates/superzej-svc/src/lsp/` is an existing
  LSP/DAP-style JSON-RPC client seam — the model for the ACP client/agent
  transport; `src/bin/fake_lsp.rs` is the fixture pattern for an ACP test
  double.
- **Harness embedding / agent role**: `crates/superzej-host/src/apps/agent.rs`
  (`AppTile`/`AgentRuntime`, `mint_proxy_key`, `SandboxTerminalTool`) — R2 wraps
  the same `AgentRuntime`; the terminal surface reuses `sandbox::enter_argv`.
- **Permission + elicitation UI**: native iocraft palette (`src/palette/`).
- **Diff rendering**: existing diff/review pane (T 260, `panel/`,
  `diff_highlight`) ingests ACP `tool_call` diff content directly.
- **Session lifecycle ↔ persistence**: worktree-tab + session
  snapshot/resurrection (DB `tab_groups`/`session_state`) maps to
  `session/resume`/`fork`; align with the time-travel-replay spec.
- **Provider seam**: DB `put_proxy_virtual_key`/`mint_proxy_key` + `szproxy` are
  already the `providers/set` target.
- **Events**: `EventBus` (`AgentDone`/`AgentFailed`) carries ACP session updates.
