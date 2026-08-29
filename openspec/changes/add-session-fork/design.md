# Design — session fork

## Fork semantics, precisely

`fork(source, opts)` is `open(spec')` where `spec'` is derived from the
source's **retained resolved recipe**:

| Field        | Fork behavior                                                                                                                                                                                                                                                               |
| ------------ | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| argv         | Source's argv, unless the source was `agent:`-launched (below)                                                                                                                                                                                                              |
| cwd          | Source's cwd; `--cwd` overrides; `--fork-worktree` remaps into the new worktree                                                                                                                                                                                             |
| env          | Raw-argv sessions: replay the caller-supplied pairs verbatim (the caller chose them). `agent:` sessions: re-resolve the full composition (command, sandbox, environment) at fork time — fresh credentials, current config, exactly what a newly opened agent pane would get |
| worktree     | Source's, or the newly created fork worktree                                                                                                                                                                                                                                |
| rows/cols    | The source's current geometry (a fork placed beside it will be resized by adoption anyway)                                                                                                                                                                                  |
| identity env | The daemon overwrites `THEGN_SESSION_ID`/`THEGN_CONTROL_SOCKET` as always, and adds `THEGN_FORKED_FROM=<source id>` (+ `THEGN_FORK_SCROLLBACK=<path>` when requested)                                                                                                       |

The result is a **new process** with a **new session id** and a **new pid** —
the spec has a scenario pinning that honesty. The source session is untouched
(fork never pauses, signals, or shares the source's PTY).

### Native recorded-harness source

When `harness` is present, `ForkSpec.session` is the native id selected from
`agent.sessions`, not a daemon id. Core validates that id and the closed
harness registry's `FORK` capability, then obtains the vendor command through
`Harness::fork_command`. If `agent` is also supplied, its configured provider
must match the recorded harness; otherwise the request is rejected rather than
silently launching a different provider. The selected command remains
authoritative while the host composes the configured agent's current
credentials, sandbox, and other launch context. Harnesses without `FORK`
remain explicitly reserved and never fall back to a guessed command.

### Recipe retention

Today `SessionMeta` keeps only `id/worktree/program/cwd/created_at/pid`; the
`OpenSpec` is dropped after spawn. Change: the daemon keeps the resolved spec
(argv, env pairs, cwd, worktree, agent-launch marker, `already_capped`) on the
session entry, in memory only:

- **Never persisted:** env pairs can carry credentials; the DB is a cache that
  outlives the process and moves across reboots. A session that predates the
  running daemon (none can — sessions die with the daemon) needs no recipe
  resurrection. Tombstones do NOT retain the recipe either: forking the dead
  is allowed only while the recipe is live, i.e. fork targets live sessions
  (a dead session returns the "session has exited" error naming
  `sessions.open` as the alternative).
- `already_capped` forks as `false` — the daemon re-wraps the fork with the
  resource-cap slice itself, since the original capping caller is not part of
  this spawn.

### Scrollback hand-off (`--scrollback`)

The daemon writes the source's retained scrollback tail (the same
`SNAPSHOT_HISTORY_LINES`-bounded text the warm-attach snapshot carries,
rendered as plain rows) to
`$XDG_STATE_HOME/thegn/forks/<new-session-id>.txt`, 0600, before spawning,
and sets `THEGN_FORK_SCROLLBACK`. The file is best-effort deleted when the
forked session exits (tombstone burial). Plain text, not a cast: the consumer
is a program reading context, not a player.

### Worktree fork composition

`--fork-worktree` (CLI) / the UI flow run in two steps with distinct failure
domains: (1) create the worktree branched from the source's (existing
creation path — git stays the source of truth; on failure nothing was
forked); (2) `sessions.fork` with cwd/worktree remapped (on failure the new
worktree remains — reported, not rolled back; deleting a worktree is a
destructive act thegn does not take implicitly). cwd remapping translates a
cwd inside the source worktree to the same relative path in the new one,
falling back to the new root.

### Placement

Fork reuses the `adopt_session` intent (as `OpenSpec.adopt` does): a running
compositor grafts the fork as a split beside the source's pane; `--tab`
requests a tab instead. Headless (no compositor attached), the session simply
exists in the daemon — visible in `thegn session list`, adoptable later.
After add-runtime-session-split lands, adoption becomes an `apply_layout`
mutation; fork itself does not change.

## Surface and wire contract

The catalog has exactly one row, `sessions.fork`, mapped to
`Verb::ForkSession`, the same write scope as `sessions.open`, and
`SurfaceSet::ALL`. The operation is non-streaming and is projected by HTTP
(`POST /v1/sessions/fork`), gRPC (`ForkSession`), the CLI, the MCP tool
`sessions_fork`, and the plugin generic capability route. MCP uses flat,
scope-checked arguments and adds no raw `argv` or arbitrary `env`.

The final request fields are `session`, optional `harness`, `agent`, `cwd`, and
`worktree`, plus boolean `scrollback`, `adopt`, and `tab`. `session` is a live
daemon id when `harness` is absent and a native harness id otherwise. The
response is `SessionInfo` with additive optional `forked_from`; it never
contains a recipe, environment, prompt, transcript, or credentials. The
control-schema JSON snapshot is generated from these wire types.

## Event-loop / render impact

None on the fork path itself: fork is a daemon-side operation reached over
the control API (off-loop by construction). The compositor sees it as the
existing adopt-intent flow — channel + waker pulse, chrome damage → `Full`
frame (the sanctioned path). No new polling anywhere.

## Alternatives considered

- **Fork = duplicate emulator + scrollback into the new pane** — rejected:
  fabricates history a different process never wrote; confuses "what did this
  process print" forensics and recording.
- **Persist recipes to the DB so dead sessions can fork** — rejected:
  persists credentials; `sessions.open` already covers "start the same
  command again" for the dead case.
- **A generic per-agent `fork_command` template** (render a resume command à
  la `agent_task`) — deferred, not rejected: `[[agents]]` config could later
  gain an optional fork/resume template rendered with `{forked_from}` /
  `{scrollback_path}`; the env-var contract here is deliberately sufficient
  for that to layer on without protocol change. Kept out of v1 to keep the
  surface minimal.
- **CRIU/process checkpointing** — out of scope permanently for the shell:
  platform-specific, sandbox-hostile, and dishonest about fd/socket state.

## Security

- **Env replay is the sharp edge.** A raw-argv session's env pairs (possibly
  containing tokens the caller injected) are replayed into the fork. This is
  the same trust level as `sessions.open` (the pairs came through the same
  scoped door), and fork requires the same scope as open — no privilege
  amplification. `agent:` launches never replay: composition is re-resolved
  fresh, so rotated/revoked credentials do not resurrect.
- **Recipes live only in daemon memory**; never in the DB, never in
  tombstones, never in any API response (fork returns `SessionInfo`, which
  carries no env).
- **Scrollback files** are terminal output (may contain printed secrets):
  0600 under the per-profile state dir, path exposed only in the forked
  process's own env, best-effort cleanup at fork exit. `--scrollback` is
  opt-in.
- **Profile firewall:** a fork always lands in the same profile/daemon as its
  source — the recipe (and its credentials) never crosses a profile boundary
  (cross-profile movement is add-session-profile-migration's cold-move, which
  moves no credentials).
- **Sandbox:** the fork passes through the same sandbox/cap wrapping as a
  fresh open (`already_capped` reset); a sandboxed source cannot fork into an
  unsandboxed sibling.
- **Blast radius:** one new write surface (spawn a process), identical in
  power to the existing `sessions.open`; `sessions.fork` is exposed on HTTP,
  gRPC, CLI, MCP, and plugin generic calls through the single catalog row and
  uses the same scope/auth gates as `sessions.open`.

## Open questions

- Should fork of a session whose _worktree is gone_ (deleted since open)
  refuse or fall back to `$HOME`? Lean refuse with a clear message.
- `--scrollback` size: fixed at the snapshot bound (2000 lines) or a
  `--scrollback-lines n` knob? Lean fixed for v1.
- Should the UI offer "fork with worktree" as the default gesture on a
  worktree pane (the orca mental model), or plain fork? Lean: pane menu
  offers both, palette action prompts.
