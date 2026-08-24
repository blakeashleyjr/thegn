# MCP write tools for the pane daemon

## Summary

`thegn mcp serve` today is read-only: `STATE_TOOLS`
(`crates/thegn-core/src/mcp/state.rs`) lists four listing tools
(`sessions_list`, `worktrees_list`, `leases_list`, `me`), all `Scope::Read`,
all argument-free. A coding agent connected to it can _see_ the daemon's
sessions but cannot _drive_ one — it cannot open a session, type into it, wait
for it to reach a state, or kill it — even though the capability catalog
already claims those verbs for the MCP surface (`sessions.open`,
`sessions.input`, `sessions.wait`, `sessions.kill` all carry `SurfaceSet::ALL`,
which includes `Surface::Mcp`), presently excused in `SURFACE_GAPS` with the
reason "MCP state tools land in the client-API phase".

This change adds four write-capable MCP tools — `sessions_open`,
`sessions_input`, `sessions_wait`, `sessions_kill` — backed by the pane
daemon, which already implements the underlying `OpenSession`/`SendInput`/
`Wait`/`KillSession` verbs on the HTTP and CLI surfaces
(`ControlClient::open`/`send_input`/`wait`/`kill` in
`crates/thegn-svc/src/control/client.rs`; no new daemon supervision logic —
this is a new door onto existing capability, reached the same way
`thegn session send|wait` and `thegn api call sessions.kill` already reach
it).

Because these tools take arguments and `StateToolSpec` today has no schema
field ("the argument schema is uniform today ... per-tool schemas come with
the first parameterised tool" — `state.rs:24`), this change also adds the
argument-schema and validation machinery `StateToolSpec` needs to stop being
schema-free — every declared tool argument is validated against its schema at
the router boundary before the daemon fetch runs, and a mismatch is a
JSON-RPC `-32602 Invalid params` error, never a daemon round-trip with bad
data. It also extends the router's permission model past the existing
scope-only gate for the one tool (`sessions_input`) whose blast radius —
arbitrary byte injection into a live terminal, including control characters —
argues for an additional, explicit interlock (see `design.md`).

Every state tool remains **default-deny**: `thegn mcp serve` with no flags
grants `read` only, so none of the four new tools is listed or callable until
the operator opts in via `--scopes` (and, for `sessions_input` specifically,
also passes `--allow-session-input`). Discovery and invocation are both
gated — an ungranted tool is neither listed by `tools/list` nor callable by
`tools/call`.

## Impact

- Roadmap: `tasks.md` **A.6** (one core, many front doors — the capability
  catalog's MCP-surface claim exists specifically so a new door like this one
  is "implement or excuse," never silent) and **AL.456** (Tools — action
  verbs; the docs/read-only tool set already shipped, this is the first
  action-verb / write set).
- Spec: `control-plane` — MODIFIED "MCP serves scope-gated state tools" to
  cover mutating tools, per-tool argument schemas, and the `sessions_input`
  interlock. `capability-catalog` — MODIFIED coverage: four `SURFACE_GAPS`
  rows for `Surface::Mcp` are retired (`sessions.open`, `sessions.input`,
  `sessions.wait`, `sessions.kill`).
- Code: `crates/thegn-core/src/mcp/state.rs` (`StateToolSpec` gains a
  declared argument schema + a pure validator; four new
  `STATE_TOOLS`/`MCP_STATE_CAPS` rows; an audit `tracing` event at the
  `call()` chokepoint). `crates/thegn-host/src/cmd/mcp.rs` (`Action::Serve`
  gains `--allow-session-input`; `fetch_state` grows four daemon-backed arms
  calling the existing `ControlClient` methods; `allowed_state_caps` grows
  the interlock). `docs/cli.md` (the `mcp serve` table — no longer purely
  read-only; document the new flags and tools).
- Out of scope (see design.md "Left out"): `sessions.split`,
  `worktrees.open`, `git.stage`/`git.commit` remain excused in
  `SURFACE_GAPS` — the schema/validation/interlock machinery this change adds
  makes adding them a small follow-up, not a redesign.

## Non-goals

- No change to `required_scope` for any existing `Verb` — the scope policy
  table (`control.rs`) is the one policy source and stays exactly as pinned by
  `verb_scope_table_is_exhaustive_and_least_privilege`. `sessions_input`'s
  extra gate is additive, checked alongside (never instead of) `write` scope.
- No new `ControlApi` methods, no new daemon supervision, no reimplementation
  of agent-launch resolution — `OpenSession`, `SendInput`, `Wait`,
  `KillSession` are already implemented and already reachable from
  `thegn session send|wait|split` and `thegn api call`.
- No change to the AI-free-shell invariant: the write tools are inert until an
  operator explicitly grants scope (and, for input, the extra flag) when
  launching `thegn mcp serve` — nothing thegn does unprompted changes.
