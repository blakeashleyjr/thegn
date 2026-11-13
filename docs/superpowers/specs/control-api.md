# The thegn control API (v1)

The contract a current tool or future desktop/web/mobile thin client builds
against. Thegn ships no first-party companion application. One service seam
(`thegn_svc::control::ControlApi`, implemented by
the pane daemon) exposed over three transports:

- **HTTP + WebSocket** (primary): the routes below, served on the daemon's
  owner-only unix socket. With `[serve] local_admin = true`, the listener reads
  native credentials from each accepted stream and grants implicit admin only
  when the peer effective UID equals the daemon effective UID. A mismatch or
  unavailable credential must present an ordinary scoped bearer token. Set
  `local_admin = false` to require tokens for every Unix-socket request (for
  example, `curl --unix-socket PATH -H 'Authorization: Bearer …'`); TCP always
  requires a bearer token. Native TCP is plaintext, so direct mode is
  loopback-only by default. A remote deployment selects `tls-terminated` or
  `tunnel`; both keep the backend on loopback and derive every advertised
  HTTP/WS/gRPC scheme from that single policy. See
  [Remote control transport security](control-transport-security.md).
- **SSE** (`GET /v1/events/sse`): the generic broadcast feed as JSON
  envelopes—a curl-friendly convenience. Pane snapshots/deltas use the
  per-session attach stream, not this endpoint; WS is primary.
- **gRPC** (`thegn.control.v1.Control`, same TCP port): a mechanical
  mirror for external tooling. See
  `crates/thegn-svc/proto/thegn/control/v1/control.proto`.

Transport security (v1): Thegn's native TCP backend is plaintext and direct
mode is therefore loopback-only. Remote access declares either a same-host TLS
terminator or an encrypted tunnel; both retain a loopback backend. Direct
non-loopback plaintext requires the explicitly unsafe global setting or CLI
flag and is reported as unsafe. Every TCP request remains token-gated.

## Scopes

Every token holds a scope set (csv in `pairings.scope`). The verb→scope table
is `thegn_core::control::required_scope` — the single tested policy source.

| Scope       | Grants                                                                                              |
| ----------- | --------------------------------------------------------------------------------------------------- |
| `read`      | list sessions/leases, snapshots, `/v1/me`, event feed, git status, bounded preview fetch            |
| `write`     | open/attach/detach/kill sessions, terminal input, resize, open-worktree (implies read)              |
| `git`       | stage + commit through the GitBackend seam (implies read, **not** write); also allows merge enqueue |
| `exec`      | run a trusted configured tool or launch preset (implies read, not write or git)                     |
| `merge_add` | enqueue a worktree for merge (no read surface; independent of write and git)                        |
| `admin`     | pairing management, daemon shutdown (implies everything)                                            |

`git`, `write`, and `exec` are independent: a companion that can commit must
not be able to type into terminals or execute configured tools. Read-only
views require only `read`—an under-scoped request is rejected **before any
action runs** (403 / `PERMISSION_DENIED`).

Worked request examples: [control-api-examples.md](control-api-examples.md).

## Transports and access modes

Which credential a caller needs depends on how it reaches the daemon. The
decision record is [Remote control transport
security](control-transport-security.md); the exposure names below are the
diagnostics `thegn doctor` reports.

| Mode                                                                     | Who it is for                                                     | Credential               | Transport schemes                 | Status                     |
| ------------------------------------------------------------------------ | ----------------------------------------------------------------- | ------------------------ | --------------------------------- | -------------------------- |
| Local-admin unix socket (`[serve] local_admin = true`)                   | The owner's own processes on the daemon host (same effective UID) | Implicit admin, no token | `http`, `ws`                      | Supported                  |
| Scoped token on the unix socket (`local_admin = false`, or UID mismatch) | Other local users and processes                                   | Scoped bearer token      | `http`, `ws`                      | Supported                  |
| `safe-loopback`                                                          | Direct TCP, loopback only                                         | Scoped bearer token      | `http`, `ws`, `grpc`              | Supported                  |
| `tls-terminated`                                                         | An operator TLS terminator on the same host as the daemon         | Scoped bearer token      | `https`, `wss`, `grpcs`           | Supported                  |
| `tunnel`                                                                 | An operator tunnel to a loopback backend                          | Scoped bearer token      | `http`, `ws`, `grpc` (client end) | Supported                  |
| `unsafe-plaintext`                                                       | Explicit unsafe opt-in for plaintext non-loopback TCP             | Scoped bearer token      | `http`, `ws`, `grpc`              | Supported, reported unsafe |
| Unsupported: native TLS termination                                      | Nobody: the native TCP backend is plaintext                       | n/a                      | n/a                               | Unsupported                |
| Unsupported: a terminator or proxy on another machine than the backend   | Nobody: the backend must stay on loopback                         | n/a                      | n/a                               | Unsupported                |
| Unsupported: trusting `Forwarded` or `X-Forwarded-*` headers             | Nobody: the daemon does not derive identity or scheme from them   | n/a                      | n/a                               | Unsupported                |

## Tokens & pairing

- Control token: `tgc1_<id:8hex>_<secret:64hex>` — the bearer credential
  (`Authorization: Bearer …` or `x-api-key`). Only `sha256(secret)` is stored.
- Pairing code: `tgp1_…` — single-use, short-TTL, embedded in a pairing URL:
  - app scheme: `thegn://pair?host=H&port=P&t=tgp1_…[&secure=1][&fp=…]`
  - web form: `http[s]://H:P/pair#t=tgp1_…` (fragment ⇒ never in server logs)
  - `secure=1` and HTTPS are emitted only for the declared TLS-terminated
    topology; tunnel mode correctly advertises its client-local HTTP endpoint.
- Redeem: `POST /v1/pair {code, label}` (unauthenticated — possession of the
  single-use code is the credential) → `{token, pairing_id, scopes, approved}`.
  With `[serve] require_approval` the token parks (`approved: false`) until
  `thegn pair approve <id>` / an in-app approval.
- Revoke: `DELETE /v1/pairings/{id}` (admin) or `thegn pair revoke <id>`.

## Routes

This table is generated from the route registry, the capability catalog and
`ROUTE_TYPES`; do not edit it by hand. A test regenerates it and fails on drift:
`THEGN_UPDATE_SNAPSHOTS=1 cargo test -p thegn-svc control::route_docs`. Stability
and Surfaces are also test-checked against the capability catalog.

<!-- BEGIN GENERATED ROUTES -->

| Route                                        | Capability                      | Scope       | Stability | Surfaces                   | Request                            | Response                                                   | Notes                                                                                                             |
| -------------------------------------------- | ------------------------------- | ----------- | --------- | -------------------------- | ---------------------------------- | ---------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------- |
| `GET /health`                                | `—`                             | `—`         | —         | —                          | —                                  | —                                                          | liveness                                                                                                          |
| `GET /pair`                                  | `—`                             | `—`         | —         | —                          | —                                  | —                                                          | static pairing redeem page                                                                                        |
| `GET /v1/agent/sessions`                     | `agent.sessions`                | `read`      | `stable`  | `http cli mcp plugin`      | `AgentSessionsQuery`               | `{sessions:[SessionRecord]}`                               | List discovered coding-agent sessions (harness, id, worktree, mtime, summary)                                     |
| `GET /v1/automations`                        | `automations.list`              | `read`      | `stable`  | `http grpc cli mcp plugin` | —                                  | `{rules:[AutomationRuleInfo]}`                             | List trusted automation rules and recent audited outcomes                                                         |
| `POST /v1/automations/test`                  | `automations.test`              | `read`      | `stable`  | `http grpc cli mcp plugin` | `AutomationTestRequest`            | `AutomationTestReply`                                      | Purely evaluate one automation rule against an event fixture                                                      |
| `GET /v1/calendar/clocks`                    | `calendar.clocks`               | `read`      | `stable`  | `http grpc cli mcp plugin` | —                                  | `{clocks}`                                                 | Resolved world clocks                                                                                             |
| `GET /v1/calendar/events`                    | `calendar.events`               | `read`      | `stable`  | `http grpc cli mcp plugin` | `CalendarQuery`                    | `{events, skipped_rows, incomplete}`                       | Merged calendar events over a date window                                                                         |
| `POST /v1/calendar/sources/{account}/events` | `calendar.ingest`               | `write`     | `stable`  | `http grpc cli plugin`     | `CalendarIngestBody`               | `{stored}`                                                 | Push events into a calendar source's own cache                                                                    |
| `GET /v1/ci/logs`                            | `ci.logs`                       | `read`      | `stable`  | `http cli mcp`             | `{worktree, run, job, tail_lines}` | `CiLogsReply`                                              | Bounded, redacted cached CI job logs                                                                              |
| `GET /v1/ci/runs`                            | `ci.runs`                       | `read`      | `stable`  | `http cli mcp`             | `{worktree, limit}`                | `CiRunsReply`                                              | Cached CI run and job metadata for a worktree                                                                     |
| `POST /v1/daemon/shutdown`                   | `daemon.shutdown`               | `admin`     | `stable`  | `http cli`                 | —                                  | `{shutdown}`                                               | Shut the daemon down                                                                                              |
| `GET /v1/dispatches`                         | `dispatches.list`               | `read`      | `stable`  | `http cli mcp plugin`      | `DispatchesQuery`                  | `{dispatches, truncated, next_before}`                     | List the agent-dispatch roster (issue, worktree, agent, status)                                                   |
| `POST /v1/dispatches`                        | `dispatches.put`                | `write`     | `stable`  | `http cli plugin`          | `DispatchPutReq`                   | `{AgentDispatch}`                                          | Record a new dispatch on the roster                                                                               |
| `POST /v1/dispatches/{id}/note`              | `dispatches.note`               | `write`     | `stable`  | `http cli`                 | `{text, token}`                    | `{id, note_id, created_at_ms}`                             | Append a progress note; the HTTP door requires the row's worker capability token                                  |
| `POST /v1/dispatches/{id}/report`            | `dispatches.report`             | `write`     | `stable`  | `http cli`                 | `{text, token}`                    | `{id, bytes, pass_without_gate_evidence}`                  | File a worker's structured handoff summary; the HTTP door requires the row's worker capability token              |
| `POST /v1/dispatches/{id}/status`            | `dispatches.set_status`         | `write`     | `stable`  | `http cli plugin`          | `DispatchStatusBody`               | `{id, status}`                                             | Advance a dispatch's status on the roster                                                                         |
| `POST /v1/editor/open`                       | `editor.open`                   | `write`     | `stable`  | `http grpc cli mcp plugin` | `EditorOpenRequest`                | `{queued}`                                                 | Queue a worktree or worktree-relative file for local editor handoff                                               |
| `GET /v1/events`                             | `events.subscribe`              | `read`      | `stable`  | `http grpc cli plugin`     | `EventsQuery`                      | `{stream}`                                                 | The live event feed                                                                                               |
| `GET /v1/events/sse`                         | `events.subscribe`              | `read`      | `stable`  | `http grpc cli plugin`     | `EventsQuery`                      | `{stream}`                                                 | The live event feed                                                                                               |
| `POST /v1/git/commit`                        | `git.commit`                    | `git`       | `stable`  | `http grpc cli plugin`     | `CommitBody`                       | `{commit}`                                                 | Commit staged changes in a worktree                                                                               |
| `POST /v1/git/stage`                         | `git.stage`                     | `git`       | `stable`  | `http grpc cli plugin`     | `StageBody`                        | `{staged}`                                                 | Stage paths in a worktree                                                                                         |
| `GET /v1/git/status`                         | `git.status`                    | `read`      | `stable`  | `http grpc cli mcp plugin` | `WorktreeQuery`                    | `{files:[GitFileStatus]}`                                  | Changed files in a worktree                                                                                       |
| `GET /v1/issues`                             | `issues.list`                   | `read`      | `stable`  | `http grpc cli mcp plugin` | `IssuesQuery`                      | `{issues, complete, incomplete}`                           | List tracker issues (filter by status/limit) from the configured provider                                         |
| `GET /v1/issues/{id}`                        | `issues.get`                    | `read`      | `stable`  | `http grpc cli mcp plugin` | `IssueRepoQuery`                   | `{IssueDetail}`                                            | Read one tracker issue with its detail and comments                                                               |
| `POST /v1/issues/{id}`                       | `issues.update`                 | `write`     | `stable`  | `http cli plugin`          | `{IssuePatch}`                     | `{Issue}`                                                  | Patch a tracker issue (status/assignee/priority/title)                                                            |
| `POST /v1/issues/{id}/comment`               | `issues.comment`                | `write`     | `stable`  | `http cli plugin`          | `CommentBody`                      | `{commented}`                                              | Post a comment on a tracker issue                                                                                 |
| `GET /v1/leases`                             | `leases.list`                   | `read`      | `stable`  | `http grpc cli mcp plugin` | —                                  | `{leases:[{lease_id, session, kind, client, expires_at}]}` | Relay lease state per session                                                                                     |
| `POST /v1/mcp_proxy/reload`                  | `mcp_proxy.reload`              | `write`     | `stable`  | `http grpc cli`            | —                                  | `{actions, tools_changed}`                                 | Re-read config and reconcile the mcp-proxy hub's upstreams (start/stop/restart/refilter)                          |
| `GET /v1/mcp_proxy/status`                   | `mcp_proxy.status`              | `read`      | `stable`  | `http grpc cli`            | —                                  | `{enabled, daemon_owned, upstreams}`                       | mcp-proxy hub state: per-upstream instances, breaker + health, exposed tool counts, withheld reasons              |
| `GET /v1/me`                                 | `me`                            | `read`      | `stable`  | `http grpc cli mcp plugin` | —                                  | `{pairing_id, label, scopes}`                              | The caller's pairing id, label and scopes                                                                         |
| `POST /v1/merge/add`                         | `merge.add`                     | `merge_add` | `stable`  | `http grpc cli plugin`     | `MergeBody`                        | `{queued, message}`                                        | Enqueue a worktree's branch on the merge queue                                                                    |
| `POST /v1/merge/clear`                       | `merge.clear`                   | `git`       | `stable`  | `http grpc cli plugin`     | `MergeBody`                        | `{cleared}`                                                | Clear the merge queue for a worktree's repo                                                                       |
| `GET /v1/merge/list`                         | `merge.list`                    | `read`      | `stable`  | `http grpc cli mcp plugin` | `WorktreeQuery`                    | `{queue}`                                                  | The merge queue for a worktree's repo                                                                             |
| `POST /v1/notify`                            | `notify.push`                   | `write`     | `stable`  | `http grpc cli plugin`     | `PushedNote`                       | `{id}`                                                     | Push a notification into the tray                                                                                 |
| `POST /v1/pair`                              | `—`                             | `—`         | —         | —                          | —                                  | —                                                          | redeem a pairing code (unauthenticated; possession of the single-use code is the credential)                      |
| `GET /v1/pairings`                           | `pairings.list`                 | `admin`     | `stable`  | `http cli`                 | —                                  | `{pairings}`                                               | List pairings                                                                                                     |
| `POST /v1/pairings`                          | `pairings.issue`                | `admin`     | `stable`  | `http cli`                 | `IssueBody`                        | `{pairing_id, code, scopes, expires_at}`                   | Mint a single-use pairing code                                                                                    |
| `DELETE /v1/pairings/{id}`                   | `pairings.revoke`               | `admin`     | `stable`  | `http cli`                 | —                                  | `{revoked}`                                                | Revoke a pairing                                                                                                  |
| `POST /v1/pairings/{id}/approve`             | `pairings.approve`              | `admin`     | `stable`  | `http cli`                 | —                                  | `{approved}`                                               | Approve a parked pairing                                                                                          |
| `GET /v1/plugins/status`                     | `plugins.status`                | `read`      | `stable`  | `http cli`                 | —                                  | `PluginsStatusReply`                                       | Per-plugin dispatcher, event-queue and provider-bridge counters published by the UI process                       |
| `GET /v1/pr/status`                          | `pr.status`                     | `read`      | `stable`  | `http grpc cli mcp plugin` | —                                  | `{prs:[PrStatusRow]}`                                      | Cached PR status for a worktree                                                                                   |
| `POST /v1/preview/fetch`                     | `preview.fetch`                 | `read`      | `stable`  | `http grpc cli mcp plugin` | `PreviewFetchRequest`              | `PreviewFetchReply`                                        | Fetch a preview URL with bounded, credential-free HTTP                                                            |
| `GET /v1/sessions`                           | `sessions.list`                 | `read`      | `stable`  | `http grpc cli mcp plugin` | —                                  | `{sessions:[SessionInfo]}`                                 | List daemon sessions (worktree hints, geometry, lease state)                                                      |
| `POST /v1/sessions`                          | `sessions.open`                 | `write`     | `stable`  | `http grpc cli mcp plugin` | `OpenSpec`                         | `SessionInfo`                                              | Open a session: argv, cwd, env, rows/cols, optional worktree                                                      |
| `POST /v1/sessions/fork`                     | `sessions.fork`                 | `write`     | `stable`  | `http grpc cli mcp plugin` | `ForkSpec`                         | `SessionInfo`                                              | Fork a live daemon or recorded harness session                                                                    |
| `DELETE /v1/sessions/{s}`                    | `sessions.kill`                 | `write`     | `stable`  | `http grpc cli mcp plugin` | —                                  | `{killed}`                                                 | Kill a session's process                                                                                          |
| `GET /v1/sessions/{s}/attach`                | `sessions.attach`               | `write`     | `stable`  | `http grpc cli`            | `AttachQuery`                      | `{stream}`                                                 | Attach to a session's live output stream (snapshot + deltas)                                                      |
| `POST /v1/sessions/{s}/detach`               | `sessions.detach`               | `write`     | `stable`  | `http grpc cli plugin`     | `DetachBody`                       | `{detached}`                                               | Detach this client from a session                                                                                 |
| `POST /v1/sessions/{s}/input`                | `sessions.input`                | `write`     | `stable`  | `http grpc cli mcp plugin` | `InputBody`                        | `{sent}`                                                   | Send terminal input bytes to a session                                                                            |
| `POST /v1/sessions/{s}/record`               | `sessions.record`               | `write`     | `stable`  | `http cli`                 | `RecordSpec`                       | `RecordStatus`                                             | Start/stop/query an asciicast recording of a session's output                                                     |
| `POST /v1/sessions/{s}/resize`               | `sessions.resize`               | `write`     | `stable`  | `http grpc cli plugin`     | `ResizeBody`                       | `{resized}`                                                | Resize a session's PTY                                                                                            |
| `GET /v1/sessions/{s}/snapshot`              | `sessions.snapshot`             | `read`      | `stable`  | `http grpc cli plugin`     | —                                  | `{session, seq, cols, rows, ansi_b64}`                     | Current emulator screen of a session                                                                              |
| `POST /v1/sessions/{s}/split`                | `sessions.split`                | `write`     | `stable`  | `http grpc cli plugin`     | `SplitBody`                        | `SessionInfo`                                              | Create a sibling pane/session next to an existing one                                                             |
| `POST /v1/sessions/{s}/wait`                 | `sessions.wait`                 | `read`      | `stable`  | `http grpc cli mcp plugin` | `WaitBody`                         | `WaitOutcome`                                              | Block until a session reaches a state (idle, exit, pattern)                                                       |
| `GET /v1/skills`                             | `skills.list`                   | `read`      | `stable`  | `http cli`                 | —                                  | `SkillsList`                                               | List embedded and configured skill metadata                                                                       |
| `POST /v1/tools/run`                         | `tools.run`                     | `exec`      | `stable`  | `http grpc cli mcp plugin` | `ToolRunRequest`                   | `SessionInfo`                                              | Run one trusted configured tool by name in a worktree                                                             |
| `GET /v1/worktrees`                          | `worktrees.list`                | `read`      | `stable`  | `http grpc cli mcp plugin` | —                                  | `{worktrees:[WorktreeInfo]}`                               | Worktrees registered with thegn (path, branch, repo root, location)                                               |
| `POST /v1/worktrees`                         | `worktrees.create`              | `git`       | `stable`  | `http cli plugin`          | `WorktreeCreateReq`                | `WorktreeInfo`                                             | Create a worktree, optionally from an issue (branch from its hint, link it)                                       |
| `POST /v1/worktrees/folder`                  | `folders.assign`                | `git`       | `stable`  | `http grpc cli mcp plugin` | `FolderAssignReq`                  | `{ok}`                                                     | Assign or clear a worktree's repo-local sidebar folder                                                            |
| `POST /v1/worktrees/open`                    | `worktrees.open`                | `write`     | `stable`  | `http grpc cli plugin`     | `OpenWorktreeBody`                 | `{opened}`                                                 | Open/focus a worktree in the owning instance                                                                      |
| `GET /v1/worktrees/sandbox-profile`          | `worktrees.sandbox_profile`     | `read`      | `stable`  | `http cli mcp`             | `SandboxProfileQuery`              | `SandboxProfileReply`                                      | Read a worktree's pinned sandbox profile (null = inherit configured policy)                                       |
| `POST /v1/worktrees/sandbox-profile/set`     | `worktrees.sandbox_profile_set` | `admin`     | `stable`  | `http cli`                 | `SandboxProfileSetReq`             | `SandboxProfileReply`                                      | Pin a worktree's sandbox profile to standard or sealed, or clear the pin (admin; host-owned, never MCP or plugin) |
| `POST /v1/push/register`                     | `—`                             | `—`         | `—`       | —                          | —                                  | —                                                          | **reserved** for push notifications; absent in v1                                                                 |

<!-- END GENERATED ROUTES -->

Browser automation is not part of v1: there is no browser provider or session
lifecycle to navigate. Local preview discovery and the UI's external-browser
open action remain product features, and `preview.fetch` remains the bounded
control operation. A future browser-driving API requires a new provider-backed
contract rather than reserving an inert route.

## The event wire

WS binary messages carry `thegn_core::control_wire::EventFrame`
(`[tag:u8][len:u32 BE][payload]`, 1 MiB cap): `Hello` (proto version, server,
your scopes), `PaneSnapshot` (ANSI repaint at `seq`), `PaneDelta` (raw PTY
bytes, `seq`), `Activity` (JSON), `Lease` (opened/refreshed/released/reaped),
`Pairing` (requested/approved/revoked), `Sessions` (re-list), `SessionExit`,
and opt-in `Lagged { missed }`.

The generic event subscription parser accepts only the bounded `kinds`
vocabulary (`activity`, `lease`, `pairing`, `sessions`, `exit`) and an optional
session id. Pane kinds such as `snapshot` and `delta` are rejected as
`bad_request` (or gRPC `invalid_argument`); snapshots and deltas are available
only on a session attach stream. Filters are applied independently after the
shared broadcast and can only narrow the read-scoped feed; omitted filters
retain the legacy full feed. Unknown or empty kinds and overlong session ids
are also rejected as `bad_request` (or gRPC `invalid_argument`). A connection
that opts in with `signal_lag=1` receives a `Lagged` marker with the count
skipped and then continues; `Lagged` is not a `kinds` value. Connections that
omit `signal_lag` retain the legacy silent skip. These are additive v1 fields
and do not change `PROTO_VERSION`.

Warm-attach contract: the snapshot at `seq` folds all output through `seq`;
the first live delta carries `seq + 1` — no gap, no overlap. A slow consumer
is never allowed to block the PTY: its deltas drop and a fresh snapshot
resyncs it.

## Capability catalog and surface gaps

Every route above is one row of `thegn_core::capability::CATALOG` (ids like
`sessions.list`, `worktrees.open`), and the HTTP router is built from the
`ROUTES` table in `thegn-svc/src/control/routes.rs`, so a route cannot exist
without a catalog row. The gRPC mirror, the CLI's control verbs, MCP tools and
plugin `host.call`s are projections of the same list; what a surface does
**not** implement yet is recorded in `SURFACE_GAPS` (with a reason) and that
list only shrinks — adding an implementation without deleting its excuse, or
a verb without a route, fails `just test`.

## Companion surface (read-mostly)

Monitor: `GET /v1/sessions` + `/v1/leases` + the event feed, and `snapshot`
for screens — all under `read` alone. Act: `git stage/commit` under `git`.
Accounts/scopes: hold one token per pairing and switch client-side; `/v1/me`
reflects the presented token. Push: reserved (`/v1/push/register`), not in v1.

The scope-enforcement guarantees are pinned by
`crates/thegn-svc/src/control/tests.rs` (router-level, with a recording
fake proving rejected requests perform zero actions).

## Error responses

HTTP errors use the additive envelope `{ "error": "<message>", "code":
"<code>" }`. The `error` message and HTTP status retain their existing
behavior; `code` is a stable identifier from the closed vocabulary
`not_found`, `no_scope`, `conflict`, `unimplemented`, `internal`,
`unauthorized`, `bad_request`, `failed_precondition`, `resource_exhausted`,
or `unavailable`. Clients should branch on `code` rather than human-readable
prose. Older servers that omit `code` remain readable by clients, which treat
it as absent.
