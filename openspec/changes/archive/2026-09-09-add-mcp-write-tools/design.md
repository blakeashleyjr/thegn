# Design

## 1. Per-tool argument schema — `StateToolSpec` stops being schema-free

Today `StateToolSpec` is `{ cap, description }`; `tool_entries()` hands every
tool the same `inputSchema: { "type": "object", "properties": {} }`, and
`call()` passes `args` straight to the injected `FetchFn` untouched — safe
only because every implemented tool ignores its arguments.

**New shape** (`crates/thegn-core/src/mcp/state.rs`):

```rust
pub enum ArgKind { String, Integer, Boolean, StringArray, Object }

pub struct ArgSpec {
    pub name: &'static str,
    pub kind: ArgKind,
    pub required: bool,
    pub description: &'static str,
}

pub struct StateToolSpec {
    pub cap: &'static str,
    pub description: &'static str,
    pub args: &'static [ArgSpec],   // NEW — &[] for the four existing read tools
}
```

`ArgKind` is deliberately a closed, flat enum, not a general JSON-Schema
builder: every daemon-backed verb's arguments are a flat object of scalars
plus at most one array (`argv: Vec<String>`) and one loosely-typed map
(`env`). A recursive schema type would buy nothing here and would be another
thing to keep pure/tested; `ArgKind::Object` is the deliberate escape hatch
for `env` — validated as "is a JSON object," with per-value string-ness
checked in the host fetch closure (below), not the core schema, because that
check is specific to one tool's semantics, not a generic argument shape.

Two things derive from `&[ArgSpec]`, both pure and both unit-tested directly
(no daemon, no `StateRouter`):

- **`inputSchema` generation** (`tool_entries()`): `{ "type": "object",
"properties": { name: { "type": kind.schema_type(), "description": … },
… }, "required": [ …required names… ], "additionalProperties": false }`.
  `additionalProperties: false` is deliberate: an MCP client that sends an
  extra field is almost always a typo or a stale integration, and rejecting
  it (rather than silently ignoring it) is cheap insurance against "I passed
  `dry_run` and it did nothing" surprises.
- **`validate_args(args: &[ArgSpec], value: &Value) -> Result<(), String>`**:
  a pure function — object-or-null in, per-field required/type/unknown-key
  check, first failure wins with a message naming the field. `StateRouter`
  calls it in `call()` **before** invoking the fetch closure, right after the
  scope check and before any daemon round-trip:

  ```rust
  pub fn call(&self, name: &str, args: &Value) -> Option<Result<Value, (i32, String)>> {
      let spec = STATE_TOOLS.iter().find(|t| tool_name(t.cap) == name)?;
      if !self.allowed.contains(&spec.cap) { /* unchanged -32001 */ }
      if let Err(msg) = validate_args(spec.args, args) {
          return Some(Err((-32602, msg)));                 // NEW
      }
      audit(spec.cap, args);                                // NEW, see §4
      Some(match (self.fetch)(spec.cap, args) { … })
  }
  ```

  `-32602` is the standard JSON-RPC "Invalid params" code — distinct from the
  existing `-32001` (scope) and `-32000` (daemon/fetch error), so a client can
  tell "you can't call this" from "you called it wrong" from "the daemon
  rejected it." No unvalidated `Value` ever reaches a `FetchFn`.

  Cross-field rules that aren't expressible as one field's type (e.g.
  `sessions_input` needs _exactly one of_ `text`/`bytes_b64`) are **not**
  encoded in `ArgSpec` — they stay in the host fetch closure, which already
  has to do daemon-specific argument assembly. The daemon's own HTTP layer
  enforces the identical one-of rule for its `InputBody`
  (`crates/thegn-svc/src/control/http.rs:465-470`), so this is not new
  laxness, just not duplicated in two places at the schema layer.

This is intentionally the minimum viable schema system for this catalog of
tools, not a general MCP-schema library. If a future tool needs nested
objects or enums, that is the next parameterized-tool's problem to solve —
premature generality here would be exactly the kind of core complexity the
95%-coverage gate makes expensive to carry.

## 2. Which verbs, and their argument shapes

Four tools, matching the task's own framing ("open sessions, send input,
wait, kill"). Each is a thin JSON↔`OpenSpec`/`ControlClient` mapping — no new
daemon logic; `crates/thegn-svc/src/control/client.rs`'s
`open`/`send_input`/`wait`/`kill` already do the work (used today by `thegn
session send|wait|split` and `thegn api call`).

| MCP tool         | cap              | scope   | `ControlClient` call                   |
| ---------------- | ---------------- | ------- | -------------------------------------- |
| `sessions_open`  | `sessions.open`  | write   | `open(&OpenSpec)`                      |
| `sessions_input` | `sessions.input` | write\* | `send_input(session, bytes, enter)`    |
| `sessions_wait`  | `sessions.wait`  | read    | `wait(session, condition, timeout_ms)` |
| `sessions_kill`  | `sessions.kill`  | write   | `kill(session)`                        |

\* plus the extra interlock, §3.

`sessions_wait` is `Scope::Read` in the existing, unmodified policy table
(`Verb::Wait`'s doc comment: "Block until a session reaches a state —
observes only") — it blocks but mutates nothing, so it is enabled by the
_default_ `--scopes read` exactly like `sessions_list`. It is still a
parameterized tool (needs `session` + `condition`), which is why it needed
this change's schema machinery even though it carries no extra permission
weight.

**`sessions_open`** args: `argv` (string array), `cwd` (string), `env`
(object, var→value), `rows`/`cols` (integer, default 24×80 when omitted),
`worktree` (string), plus an agent-launch path that reuses `OpenSpec.agent:
Option<AgentLaunch>` rather than reimplementing agent resolution: `agent`
(string — an `[[agents]]`/`[[tools]]` name or provider id), `prompt`
(string), `headless` (boolean), `bind_worktree` (boolean). None are
individually `required`; the daemon already rejects empty argv with no agent
(`Conflict`, `service.rs:458`), so that cross-field rule stays server-side
same as the schema-layer one-of rules above. `adopt` and `already_capped` are
**not** exposed as tool arguments — they are hardcoded `false`. `adopt` asks
a _running compositor_ to graft the session into a real pane, which is a
local-UI concern an MCP caller has no business requesting; `already_capped`
exists only for the compositor's own already-sandboxed spawn path (the doc
comment: "Everyone else leaves this `false` and gets the cap applied for
them") — an MCP-originated open must always get the cap applied.

**`sessions_input`** args: `session` (string, required), `text` (string),
`bytes_b64` (string, base64), `enter` (boolean). Exactly one of `text`/
`bytes_b64` must be given (checked host-side, mirroring the daemon's own
`InputBody` rule); `bytes_b64` is what lets an agent send control characters
(`Ctrl-C` = `0x03`) that a UTF-8 `text` field cannot carry — the daemon
writes bytes straight to the PTY's stdin with zero interpretation
(`daemon/session.rs:325-328`), so whatever thegn decodes here executes
exactly as if typed at a keyboard connected to that pane.

**`sessions_wait`** args: `session` (string, required), `condition` (string,
required — reuses the CLI's own mini-grammar, `exited|idle|blocked|done|
match:<regex>`; `crates/thegn-host/src/cmd/session.rs`'s
`parse_wait_condition` becomes `pub(crate)` and is called from `mcp.rs`
instead of being reimplemented), `timeout_ms` (integer, omit = wait
forever).

**`sessions_kill`** args: `session` (string, required). Idempotent
server-side (`service.rs:657-672`): killing an already-dead session is `Ok`,
so a racing agent can't turn a kill into a crash.

**Left out of this change** (documented, not silently dropped):
`sessions.split` (mirrors `sessions.open`'s shape closely — the schema
machinery this change adds makes it a small follow-up once the pattern is
proven with real usage), `worktrees.open` (fires a compositor intent —
useful, but a UI-focus side effect for a headless agent caller is a
different-shaped design question: what should happen when no compositor is
running?), `git.stage`/`git.commit` (a distinct scope silo, `Scope::Git` —
worth its own pass on redaction/audit rather than folding in here). All four
remain excused in `SURFACE_GAPS` for `Surface::Mcp` after this change.

## 3. Permission model

**Baseline: `required_scope(verb)` is the only scope policy**, unchanged.
`thegn mcp serve` computes `allowed: Vec<capability id>` from `--scopes`
exactly as it does today (`allowed_state_caps` in `cmd/mcp.rs`) — a capability
is in `allowed` iff the requested `ScopeSet` satisfies `required_scope` of
its verb. `StateRouter` denies both discovery (`tool_entries()` filters by
`allowed`) and invocation (`call()` re-checks `allowed` regardless of what
was advertised) — this was already true for the four read tools and needs no
change; it composes for free with the new write tools because they route
through the same `allowed` list.

Default `thegn mcp serve` (no flags) still resolves `--scopes read`, which
now grants exactly `{sessions.list, worktrees.list, leases.list, me,
sessions.wait}` — the read-scope tools. `sessions.open`/`sessions.kill`
require `--scopes` to include `write`. **This is the deliberate decision
`every_state_cap_is_read_scope_today`'s doc comment predicted**: "this pins
that a future write-side tool forces a deliberate scope-model decision
rather than silently widening `read`" — that test is replaced by one
asserting the new split explicitly (§ tests below), not loosened.

**`sessions_input`'s extra interlock.** The task calls out its blast radius
specifically, and it deserves a harder look than "it's Write scope like the
other mutations": `sessions.open`/`sessions.kill` act on a session's
_lifecycle_ (create it, end it) — bounded, auditable, idempotent operations.
`sessions.input` acts on a session's _live input stream_ — it is, verbatim,
"type these bytes into this terminal," including control characters. Whatever
process is attached to that pane executes whatever the bytes mean to it: a
shell, an editor, another agent, a `sudo` prompt. A coding agent that can
call `sessions_input` can pivot into typing into _any_ session the daemon
happens to be running, not just ones the agent opened for itself — including
a human's own interactive pane if one happens to share the daemon. That is a
materially larger blast radius than "commit whatever is staged" (`git`
scope) or "open a new sandboxed process" (`sessions.open`), and it is a
different threat model from the control-API's existing `write` scope, whose
worked example in its own doc comment is a **paired phone** with a human
holding it, not a semi-autonomous LLM agent that can be prompt-injected by
anything the pane displays back to it.

Two options considered:

- **Promote `Verb::SendInput` to `Scope::Admin`.** Rejected: `required_scope`
  is the single, surface-agnostic policy table (control API, gRPC, CLI, MCP,
  plugins all read it) — moving `SendInput` to `Admin` would also gate the
  HTTP/CLI/mobile-companion `write`-scoped "send terminal input" affordance
  that the `Scope::Write` doc comment names as the canonical `write`-scope
  example, and would fail the pinned
  `verb_scope_table_is_exhaustive_and_least_privilege` test. It over-corrects
  for one surface's threat model by breaking every other surface's.
- **An MCP-surface-only interlock, additive to (never instead of) the
  `write` scope check.** Adopted: a new `--allow-session-input` flag on
  `thegn mcp serve` (mirroring the existing `--scopes` flag's own idiom — an
  explicit, visible-in-the-launch-command opt-in, not a sticky config file a
  user forgets is set). `sessions.input` is only added to `allowed` when
  _both_ `write` scope is granted _and_ the flag is passed:

  ```rust
  fn allowed_state_caps(scopes: ScopeSet, allow_session_input: bool) -> Vec<&'static str> {
      MCP_STATE_CAPS.iter().copied().filter(|id| {
          let Some(c) = lookup(id) else { return false };
          scopes.allows(scope_of(c))
              && (*id != "sessions.input" || allow_session_input)
      }).collect()
  }
  ```

  This is **not** a second policy table: it names exactly one capability
  (the one this design just finished arguing is qualitatively different),
  the underlying scope check still runs unconditionally, and the flag is
  visible in the same place the scope grant already is — the
  `claude mcp add thegn -- thegn mcp serve --scopes write --allow-session-input`
  command line an operator writes once. `StateRouter` itself does not know
  this interlock exists; it only ever sees the resulting `allowed` list, so
  the "single choke point" property (`call()` re-checks `allowed` regardless
  of `tools/list`) covers the interlocked tool for free — no new code path
  to keep in sync with discovery.

## 4. Audit

**Where:** `StateRouter::call()` in core — the one chokepoint every tool
invocation passes through after the scope and schema checks succeed and
before the fetch closure runs, and again after it returns. Emitted with
`tracing`, already a `thegn-core` dependency and already used directly from
core elsewhere (`msg.rs`, `activity.rs`, `db_migrate.rs`); no substrate
boundary is crossed (`tracing` is a logging facade, not an I/O backend — the
host owns whatever subscriber is installed, same as every other core trace).

**What:** every call to a **mutating** tool (`required_scope(verb) !=
Scope::Read` — i.e. `sessions_open`/`sessions_input`/`sessions_kill` today)
logs an `info`-level `target: "thegn::mcp"` event on entry (`cap`, redacted
`args`) and on exit (`cap`, `ok` or `error` + message). Read tools
(`sessions_list`, …, `sessions_wait`) are not specially audited beyond
whatever the ambient log level already captures — they observe, they don't
change anything, and logging every listing call would be noise without
being defense.

**Redaction**, a pure `redact_for_audit(cap, args) -> Value` next to
`validate_args`:

- `sessions.input`: `text`/`bytes_b64` are replaced with their byte length
  (`"<12 bytes>"`) — the audit trail proves _that_ input was sent and _how
  much_, without putting a possibly-sensitive keystroke stream (a pasted
  token, a password typed at a prompt) in a log file. `session`/`enter`
  survive unredacted (they're not secrets and are exactly what an operator
  reviewing the log needs: which pane, was Enter appended).
  - "How much" was pointed out over "how little": logging even the _length_
    of a Ctrl-C-only payload (1 byte) is itself informative and fine to
    leave visible.
- `sessions.open`: `env` is replaced with its entry count
  (`"<3 vars>"`) — env is exactly where a caller would put an API key or
  token for the launched process. `argv`/`cwd`/`worktree`/`agent`/
  `bind_worktree` survive unredacted (they name what ran and where, not a
  secret). `prompt` survives unredacted but is not expected to be
  secret-shaped (it's the agent's own task description) — if that
  assumption turns out wrong in practice, truncating/redacting it is a small
  follow-up, not a design change.
- `sessions.kill`: nothing to redact (`session` only).

This mirrors the existing `redact()` in `docs.rs` (key-name-driven secret
masking for `get_config`) in spirit — secrets never leave the process
unmasked, whether the destination is an MCP response or a log line — but is
a separate, smaller function: the docs redaction walks an arbitrary config
tree by key-name heuristic; audit redaction only ever sees one of a handful
of known, fixed-shape tool-argument objects, so it is a `match` on `cap`, not
a generic tree walk.

## 5. Tests (core, pure, no daemon)

- `validate_args`: missing required field, wrong type per `ArgKind`, unknown
  field, `Value::Null` accepted for a no-arg tool, `Value::Null` rejected for
  a tool with a required arg, an array containing a non-string rejected for
  `StringArray`.
- `tool_entries()`: a tool with args produces `required`/`properties`/
  `additionalProperties: false` matching its `ArgSpec` list; a no-arg tool's
  schema is unchanged from today's `{ "type": "object", "properties": {} }`
  (no `additionalProperties` regression on the existing four — checked
  explicitly so the shape change doesn't leak onto tools that never asked for
  it).
- `call()`: schema failure short-circuits before the fetch closure runs (a
  fetch stub that panics if invoked, called through a bad-args case, proves
  it) — the literal "unvalidated args never reach the daemon" requirement.
  `-32602` on bad args, `-32001` on missing scope (unchanged), `-32000` on a
  fetch error (unchanged).
- `allowed_state_caps` (host, `cmd/mcp.rs`): `read` alone yields exactly the
  five read-scope caps; `write` alone additionally yields
  `sessions.open`/`sessions.kill` but _not_ `sessions.input`; `write` +
  `allow_session_input=true` yields all eight; `allow_session_input=true`
  alone (no `write`) still excludes `sessions.input` (the interlock is
  additive, never a bypass).
- `redact_for_audit`: `sessions.input`'s `text`/`bytes_b64` never appear
  verbatim in the output; `sessions.open`'s `env` values never appear
  verbatim; unredacted fields survive unchanged.
