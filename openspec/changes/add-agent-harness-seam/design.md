# Design — agent harness seam

## Shape: pure knowledge in core, I/O in svc/host

The seam splits the way the usage tracker already does (pure parsers in
`thegn_core::usage`, filesystem/network in `thegn_svc::usage`):

- **`thegn_core::harness`** holds the object-safe trait and the per-harness
  _knowledge_: home resolution rules (env var, default dir), launch/resume argv
  **forms** (templates, not spawned processes), session-store **layout rules**
  (glob patterns, filename → session-id extraction), and **parsers**
  (`&[u8] → AccountUsage`, transcript line → token record, session file →
  summary). All pure, all unit-testable — this is what keeps the 95% core
  coverage gate satisfiable.
- **`thegn-svc` / `thegn-host`** drive the I/O: walking session stores, the
  opt-in live usage fetch, spawning the resolved argv, the doctor probes.
  Off-loop always; results return over a channel + `TerminalWaker` pulse
  (event-loop invariant — no new loop work; the existing usage gather and
  launch paths already have the channels).

Trait sketch (object-safe; no `async fn` in the trait — the provider-trait
ratchet):

```text
trait Harness {
    fn id(&self) -> &'static str;                  // "claude", "codex", …
    fn home(&self) -> HomeSpec;                    // env var + default dir + auth marker
    fn interactive_command(&self) -> &str;
    fn headless_template(&self) -> Option<&str>;   // "{prompt}" placeholder contract
    fn login_argv(&self) -> &[&str];
    fn caps(&self) -> HarnessCaps;                 // SESSIONS | RESUME | USAGE | TOKENS | TEAMMATES
    // optional ops, present iff the cap bit is set:
    fn session_layout(&self) -> Option<SessionLayout>;      // store globs + id/project extraction
    fn parse_session_summary(&self, bytes: &[u8]) -> Option<SessionSummary>;
    fn resume_command(&self, session_id: &str) -> Option<String>;
    fn parse_usage(&self, bytes: &[u8], now: i64) -> Option<AccountUsage>;
    fn fold_transcript(&self, bytes: &[u8], …);
}
```

The registry is a `const` table like `account::PROVIDERS` — **closed**. A
config `kind` naming an unimplemented harness is `reserved` (declared, probed
as unavailable, never executed), matching the provider-seams
implemented-or-reserved rule. Config cannot define a harness by supplying
commands: that would turn a config file into arbitrary-command execution and
cannot encode parsers anyway.

## Facet mapping (what moves where)

| Facet                     | Today                                                            | Under the seam                                                            |
| ------------------------- | ---------------------------------------------------------------- | ------------------------------------------------------------------------- |
| id/home/login/auth marker | `account::PROVIDERS`                                             | `Harness::home`/`login_argv` (PROVIDERS becomes a view or is absorbed)    |
| Headless form             | `agent_task::headless_command` match                             | `Harness::headless_template` (resolution order in `agent_task` unchanged) |
| Bare-provider launch      | `daemon/agent_open.rs::bare_provider`                            | registry lookup                                                           |
| Usage parsing             | `usage::parse_claude_usage` / `parse_codex_rollup`               | `Harness::parse_usage`                                                    |
| Session-store walk        | `thegn_svc::usage` (`codex_sessions_dir`, `collect_transcripts`) | `Harness::session_layout` + one generic walker                            |
| Token fold                | `usage_tokens::fold_transcript` (Claude-shaped)                  | `Harness::fold_transcript`                                                |
| Login-sync allowlist      | sandbox credential-carry lists                                   | `HomeSpec` auth-critical file list                                        |
| Session history           | — (missing)                                                      | `session_layout` + `parse_session_summary` → `agent.sessions`             |
| Resume                    | — (missing)                                                      | `resume_command` + `AgentLaunch.resume`                                   |
| Teammate sessions         | — (missing)                                                      | reserved cap (below)                                                      |

The retrofit is behavior-identical by construction: each existing call site
delegates through the seam and keeps its tests; the seam impls carry the moved
unit tests plus conformance tests shared across harnesses (every harness with
`RESUME` produces a command containing its session id, quoted; every
`session_layout` glob matches its own fixture; etc.).

## Session history and resume

Discovery is per-worktree: the caller passes a worktree path, the harness maps
it to its project key (Claude Code slugs the cwd into
`projects/<slug>/*.jsonl`; Codex records cwd inside the rollout), and the
walker returns `SessionRecord { harness, id, worktree, mtime, summary }`,
newest first, bounded (same file-count cap discipline as the token rollup).
Nothing is watched: this is a read-on-demand list (CLI/MCP call or overlay
open), so no new idle work exists.

`AgentLaunch.resume: Option<String>` extends the control wire — the pinned
`control_schema` snapshot is refreshed (a known ratchet). `agent_open::resolve`
swaps the headless/interactive command for `resume_command(id)` when set.
Auto-resume: on session resurrection, if the worktree's remembered agent's
harness has `RESUME`, the `[[agents]]` entry opted in (`resume = true`, default
`false`), and a session record exists for that worktree, resurrection launches
the resume form; any failure to discover falls back to the cold launch. The
`state-db` "stale agent state is coerced" contract is untouched — resume
changes what argv is spawned, not the settling rules.

## Scoped MCP control

Today `thegn mcp serve` takes `--scopes` per invocation. This change resolves
the granted scope set from config first:

```toml
[mcp.serve] scopes = ["read"]                  # global ceiling
[profile.<name>.mcp_serve] scopes = ["read"]   # profile: may only narrow
[workspace.<slug>.mcp_serve] scopes = […]      # workspace: may only narrow
```

Resolution is **clamp-only**: each inner level intersects the outer set; the
`--scopes` flag intersects the result. A repo-local overlay therefore can
never widen what the operator granted globally — the same trust direction
`add-config-trust-resolution` establishes for repo config generally; this
change consumes that model rather than inventing one. `thegn doctor` (and
`thegn mcp serve` startup output) prints the effective set and which level
clamped it.

## Alternatives considered

- **Leave it scattered.** Every new harness is an N-site sweep and session
  history/resume have no home. Rejected — this is the debt the seam pays down.
- **An ACP adapter layer** (roadmap group R). Owns the agent _conversation_,
  requires an in-process protocol client, and was excised with the AI layer.
  The harness seam is deliberately lower: filesystem + argv knowledge only, no
  protocol, no model traffic. ACP can later be an implementation detail of a
  richer harness without changing this seam's callers.
- **Config-defined harnesses** (users declare launch/resume/store patterns in
  TOML). Rejected: arbitrary-command execution from config, and parsers cannot
  be expressed. The registry stays closed; plugins (P 200) are the future
  extension door.
- **An LLM proxy as the metrics source.** Excised; THE-58 territory. Usage and
  tokens here come from what harnesses write locally, exactly like V 300 today.

## Security

- **Credential homes are read-only inputs.** The seam reads usage/session
  state from credential directories but MUST never write there, never copy
  token material into payloads, logs, or the DB, and never include
  `auth_marker`/credential file _contents_ in any op result. Paths and emails
  (already surfaced by V 300) remain the identity fields.
- **No raw tokens in config.** Nothing here adds credential config; login
  stays the harness's own `login_argv` flow (SecretRef/env:/file: rules
  unaffected).
- **Resume ids are untrusted input** (they cross MCP/HTTP/CLI). They are
  validated against the discovered id shape and pass through the existing
  `agent_task` shell-quoting contract; an id that fails validation errors —
  it is never interpolated raw.
- **Session payloads leak workspace facts** (paths, first-prompt summaries).
  `agent.sessions` is Read-scoped; MCP exposure obeys the (config-clamped)
  scope set. Summaries are truncated single lines, not transcript bodies.
- **Scope clamping is fail-closed.** An unparseable scope entry at any level
  resolves to the empty set for that level's contribution, not to "everything";
  the widest possible result is the global ceiling.
- **Sandbox**: launches (resume included) compose through the existing
  sandbox/credential path (`agent::launch_spec_full`) — no new spawn path, so
  the resource-cap and login-carry behavior is inherited, not re-implemented.
- **Blast radius**: the only new write surface is `sessions.open` gaining
  `resume` — same scope as today's `sessions.open`. Everything else added is
  Read.

## Open questions

- **Teammate mode** (Claude Code teams / cmux-style native splits with sidebar
  metadata): the on-disk/session shape of teammate sessions is not yet
  verified well enough to spec discovery, and "adopt teammates as native
  splits" needs the daemon's adopt path. The cap ships **reserved**; a
  follow-up change specs it once verified against a real teammate session.
- Whether `account::PROVIDERS` survives as a thin view (login/carry callers
  are many) or is absorbed outright — decided during implementation, invisible
  to callers either way.
- Whether `agent sessions` also lists sessions for worktrees thegn does not
  know (harness stores are host-wide). Default: yes, flagged `unlinked`, since
  toktrack/codeg show the host-wide view is the expected one.
