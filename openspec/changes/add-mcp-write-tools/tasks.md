# Tasks

## 1. Core: per-tool argument schema + validation (`thegn-core`)

- [x] 1.1 `crates/thegn-core/src/mcp/state.rs`: add `ArgKind`
      (`String`/`Integer`/`Boolean`/`StringArray`/`Object`) and `ArgSpec`
      (`name`/`kind`/`required`/`description`); add `args: &'static [ArgSpec]`
      to `StateToolSpec`; set `args: &[]` on the four existing entries
      (no behavior change for them).
- [x] 1.2 `tool_entries()`: build `inputSchema` from `args` —
      `properties`/`required`/`additionalProperties: false`; keep the
      no-arg shape (`{"type":"object","properties":{}}`) for tools with
      `args: &[]`, unit-tested explicitly so the change doesn't leak onto
      the untouched four.
- [x] 1.3 `pub fn validate_args(args: &[ArgSpec], value: &Value) -> Result<(), String>`
      — pure, object-or-null in; required/type/unknown-key checks. Unit
      tests per `ArgKind` and per failure mode (design.md §5).
- [x] 1.4 Wire `validate_args` into `StateRouter::call()` between the scope
      check and the fetch call; `-32602` on failure. Unit test: a fetch stub
      that panics if invoked proves a bad-args call never reaches it.
- [x] 1.5 `pub fn redact_for_audit(cap: &str, args: &Value) -> Value` —
      `sessions.input` (`text`/`bytes_b64` → byte-length string),
      `sessions.open` (`env` → entry-count string), identity otherwise. Unit
      tests per design.md §4/§5.
- [x] 1.6 Wire an audit `tracing::info!`/`tracing::warn!` (target
      `"thegn::mcp"`) into `StateRouter::call()` for tools whose
      `required_scope(verb) != Scope::Read`, logging `cap` +
      `redact_for_audit` output on entry and outcome on exit. No test needed
      beyond compiling (tracing has no assertable return value here); rely
      on `1.4`'s existing call-path tests to prove it doesn't change
      behavior.

## 2. Core: register the four tools

- [x] 2.1 Add `STATE_TOOLS`/`MCP_STATE_CAPS` entries: `sessions.wait`,
      `sessions.open`, `sessions.input`, `sessions.kill` (append after the
      existing four; `state_tools_match_state_caps` pins the pairing).
      Descriptions state the scope each needs, matching the existing
      four's style (`state.rs:38-57`) — `sessions_input`'s description
      additionally notes the `--allow-session-input` requirement so a
      client sees why a `write`-scoped call is still refused.
- [x] 2.2 `crates/thegn-core/src/capability.rs`: delete the four
      `Surface::Mcp` `SURFACE_GAPS` rows for `sessions.open`,
      `sessions.input`, `sessions.wait`, `sessions.kill` (leave their
      `Surface::Plugin` rows — plugin dispatch is untouched). `just quick
thegn-core` / `mcp_tools_cover_catalog` should now fail-then-pass as
      the two lists reconcile.
- [x] 2.3 Replace `every_state_cap_is_read_scope_today` with a test
      asserting the new split: `sessions.list`/`worktrees.list`/
      `leases.list`/`me`/`sessions.wait` are `Scope::Read`;
      `sessions.open`/`sessions.input`/`sessions.kill` are `Scope::Write`.
      Keep the doc comment's framing ("a deliberate decision, not a silent
      widening") on the new test.

## 3. Host: wire the daemon (`thegn-host`)

- [x] 3.1 `crates/thegn-host/src/cmd/session.rs`: make `parse_wait_condition`
      `pub(crate)` (no logic change) so `mcp.rs` reuses it instead of
      reimplementing the mini-grammar.
- [x] 3.2 `crates/thegn-host/src/cmd/mcp.rs`, `Action::Serve`: add
      `--allow-session-input` (`bool`, default `false`); doc-comment update
      explaining the interlock (mirrors the existing `--scopes` comment's
      style).
- [x] 3.3 `allowed_state_caps`: take `allow_session_input: bool`; filter
      `sessions.input` on it in addition to the scope check (design.md §3).
      Update its call site in `serve()`.
- [x] 3.4 `fetch_state`: four new match arms — - `sessions.open`: parse `OpenSpec` from args (argv/cwd/env/rows/cols/
      worktree + `agent: Option<AgentLaunch>` from agent/prompt/headless/
      bind*worktree), hardcode `adopt: false, already_capped: false`,
      call `client.open(&spec)`, return `serde_json::to_value(&info)`. - `sessions.input`: require exactly one of `text`/`bytes_b64` (clear
      error otherwise, mirroring `InputBody`'s own rule), decode
      `bytes_b64`, call `client.send_input(session, bytes, enter)`. - `sessions.wait`: `parse_wait_condition(condition)` (3.1), call
      `client.wait(session, cond, timeout_ms)`. - `sessions.kill`: call `client.kill(session)`.
      All four: `client.map_err(|*| NO_DAEMON.to_string())?`first, matching
the existing four arms' no-daemon handling (no DB-cache fallback —
these need a live daemon, same as`sessions.list`/`leases.list`/`me`).
- [x] 3.5 Update `mcp_scope_mapping_read_covers_every_state_cap` /
      `mcp_scope_mapping_none_disables_state_tools` (now false as written —
      "every scope csv gives the full set" no longer holds) with the split
      asserted in design.md §5's `allowed_state_caps` bullet.

## 4. Docs

- [x] 4.1 `docs/cli.md`'s "Docs endpoint for agents (`mcp serve`)" section:
      it currently undersells even the _existing_ read state tools (only
      lists the five docs tools) and calls the endpoint "read-only" —
      correct both: list `sessions_list`/`worktrees_list`/`leases_list`/`me`/
      `sessions_wait` under `--scopes read` (or better), and the four write
      tools under `--scopes write` (+ `--allow-session-input` for
      `sessions_input`), naming the interlock explicitly.

## 5. Spec sync

- [x] 5.1 `openspec/changes/add-mcp-write-tools/specs/control-plane/spec.md`
      — MODIFIED "MCP serves scope-gated state tools" (mutating tools,
      schema validation, the interlock).
- [x] 5.2 `capability-catalog` spec: no delta. Its requirements ("each
      surface covers the catalog or documents the gap," "a stale gap fails
      the build") already fully describe retiring a `SURFACE_GAPS` row —
      deleting the four `Surface::Mcp` rows is compliant behavior under the
      existing requirement text, not a requirement change.
- [x] 5.3 `mcp-servers` spec: no delta. That spec's Purpose is user-_declared_
      MCP servers (`[mcp_servers.<name>]`), a different capability from
      thegn's own `mcp serve` endpoint (which lives in `control-plane`).

## Validation

- [x] Scoped tests while iterating: `cargo nextest run -p thegn-core mcp::`,
      `cargo nextest run -p thegn-host mcp` (see CLAUDE.md dev-loop policy —
      do not run full-workspace gates per edit).
- [x] `just openspec-validate` once the specs/tasks are stable.
- [ ] End with a single `just ci` run (or, per repo policy, `just test` +
      `just lint` pre-push) once implementation is complete — not per-edit.
