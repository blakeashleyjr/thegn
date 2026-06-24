# Exfiltration-resistant agent mode (exploration — NOT scheduled)

Status: **exploration only.** Captured 2026-06-24 from studying
`deepbluedynamics/nemesis8`. No implementation planned; this is a parking spot
for an idea that complements (but is distinct from) the `sealed` sandbox
hardening preset.

## The idea: invert the sandbox

The usual sandbox jails the agent and tries to mediate its network egress.
nemesis8's sealed "pokeball" mode inverts this — it keeps the **brain on the
host** and puts only the **hands in the jail**:

- **Broker** (host process; holds the API key; the only thing with network) _is_
  the agent loop. It calls the LLM directly, receives `tool_use` blocks, and
  dispatches the tool calls.
- **Worker** (a binary inside a `network=none` container) executes tool calls
  (bash / file_read / file_write / grep / glob, path-jailed to the worktree) and
  returns results.
- The **only** channel between them is a bind-mounted `/comms/{inbox,outbox,
status}` directory carrying atomically-written JSON messages (`write .tmp` →
  `rename`), each side polling ~100 ms. Sequence-numbered filenames (broker from
  1, worker from 100) avoid collisions; the worker writes a heartbeat into
  `status/`.

### Why it resists exfiltration

The container has **no network** and **never holds the key**. Its only egress is
files the host chooses to read — every message is a durable, auditable artifact.
A compromised or prompt-injected agent cannot phone home, because the process
that talks to the network (the broker) is outside the blast radius and the jail
has no route out.

(nemesis8's wire protocol also defines `ToolRequest`/`ToolResult` for
host-mediated privileged ops — a worker asking the broker to perform a networked
action on its behalf — but its current worker doesn't use them; everything inside
the jail is network-free by construction.)

## Why this fits superzej specifically

- The **LLM proxy is already the single network chokepoint and key-holder**
  (`crates/superzej-core/src/config.rs` `LlmProxyConfig`;
  `crates/superzej-host/src/proxy_daemon.rs`). A broker that owns model traffic
  is most of the way built.
- `SandboxSpec` already carries `env_overrides` / `env_block`
  (`crates/superzej-core/src/sandbox.rs`) for injecting a per-agent **virtual
  key** and suppressing the master key — exactly the host-keeps-the-key shape
  this mode needs.

A "comms-broker" agent mode would therefore be: untrusted tool execution in a
`network=none` sandbox, with the proxy holding model traffic and the scoped
virtual key entirely host-side.

## Relationship to the shipped hardening presets

The `sealed` hardening preset (see the sandbox-hardening-presets work) just locks
the _container_ down — read-only root, dropped caps, `network=none`, pids/mem
caps — while the agent process still runs _inside_ it. This exploration is a
bigger structural change: it moves the **agent loop itself** out of the jail and
leaves only tool execution inside. Adopt the preset first; revisit this only if a
stronger "no key, no egress, fully audited" guarantee is needed.

## Open questions (for if/when this is picked up)

- Reuse the proxy as the broker, or a separate host-side broker process?
- Message transport: a `/comms` file queue (portable, auditable) vs a host-side
  unix socket (faster, less auditable).
- How interactive TUIs (vs one-shot prompts) map onto a turn-based broker loop.
- Whether `ToolRequest`/`ToolResult` host-mediated egress is ever desirable, or
  whether `network=none` should be absolute.
