---
id: daemon-and-sessions
title: Daemon & sessions
order: 13
actions: [detach, quit, quit-kill]
---

# Daemon & sessions

Your terminals do not belong to the UI. A background **pane daemon** owns
the PTYs, so quitting thegn — or losing the ssh connection it was running
over — leaves the work running. Reattaching resumes the same live screen,
not a replay.

## Detach vs quit

- **Detach** leaves everything running and returns you to your shell.
  Relaunching `thegn` warm-reattaches.
- **Quit** closes the UI. Daemon-backed panes survive; panes that were
  never daemon-backed do not.
- **Quit and kill** ends the sessions too — the explicit "I'm done"
  option.

The status bar's daemon chip shows whether the focused pane is
daemon-backed; select it for the full session rollup. See [[bars]].

## Live sessions from any shell

`thegn session list` shows what the daemon is holding. Each row has an id
the other verbs take:

| Command                                    | What it does                                                                    |
| ------------------------------------------ | ------------------------------------------------------------------------------- |
| `thegn session list`                       | every live session (`--json` for scripts)                                       |
| `thegn attach [id]`                        | grab one interactively; `Ctrl-\` detaches                                       |
| `thegn session attach --session <id>`      | stream its output to stdout                                                     |
| `thegn session snapshot --session <id>`    | dump its current screen (`--text` for plain rows, `--json` for geometry + ANSI) |
| `thegn session send --session <id> <text>` | type into it (`--enter` to run)                                                 |
| `thegn session wait --session <id>`        | block until it's `exited`, `idle`, `blocked`, `done`, or `match:<regex>`        |
| `thegn session record <id>`                | record its output to a `.cast` file; `--stop` finalizes, `--status` reports     |

## Recording a session

`thegn session record <id>` starts a server-side asciicast recording of one
session's output. Because the **daemon** owns it (not the UI), it keeps
recording while every client is detached — exactly when an unattended agent
session is most worth capturing. `--stop` finalizes the file; with neither flag
it reports status. The control verb is `sessions.record` (HTTP/gRPC/CLI only —
never MCP or plugins, so an agent can't silently bug another session), and it
needs a **write**-scoped token.

Recordings are terminal output and can contain whatever secrets a tool printed,
so files are written `0600` under a `0700` directory (`[recording] dir`, default
`$XDG_STATE_HOME/thegn/recordings`), the API returns only the path — never the
contents — and a `[recording] max_bytes` cap finalizes a valid file instead of
filling the disk. This is separate from the client-side whole-UI recorder
(`Ctrl-Alt-r`) and from per-pane time-travel replay.

Over the control API the same daemon also answers `GET /v1/worktrees` — the
worktrees registered with thegn (path, branch, repo root, location) — so a
thin client can list what it may open before calling `/v1/worktrees/open`.

`thegn attach` with no argument lists sessions to pick from. It is
local-only — it speaks the unix socket and never dials the TCP listener.

`session wait` is the one built for scripting: it exits 0 on match, 2 on
timeout, and 1 if there is no daemon, so a shell script can drive a long
build and block on it.

## Closing a session, and the dispatch door

`thegn session close <id>` terminates a session's PTY child. The daemon
keeps a tombstone, so `session list` still shows how it ended
(`exited(?,idle)` for a closed session) and `session wait` still answers —
the dedicated verb for what `sessions.kill` reaches generically.

`session list` marks each row with a **liveness token** in the second
column: `live`, or `exited(<code>)` — `exited(?)` when the exit code is
unreapable, suffixed with the final state word (`exited(0,done)`). A
supervisor greps a fixed column instead of parsing the whole line;
`--live` filters to non-exited rows before serialization, so a `--live
--json` caller never re-filters.

`session open --stage <name> --issue <id>` is the pipeline's one-call
dispatch: it renders the stage's prompt template, inserts the roster row,
opens the session headless, and stamps the row with the session id and the
artifact path it printed. An explicit `--prompt` is refused (the template
owns the task). `--stage` **without** `--issue` stays a plain open whose
launch layers the stage's `model` / `env` / `permissions` over the agent —
see [[configuration]]. The roster side (verify a finished row, wait on one,
gate `done`) is [[cli]]'s `dispatch verify` / `dispatch wait` /
`dispatch set-status done`.

## Serving thin clients

### Watching the event feed

`thegn events tail` is the CLI reference client for the daemon's live event
feed. It waits on the existing subscription stream rather than polling:

```sh
thegn events tail --kinds activity,exit
thegn events tail --session "$SESSION" --signal-lag --json
```

`--kinds` and `--session` use the control API's bounded narrowing vocabulary.
The greeting is always the first frame. `--signal-lag` makes dropped frames
visible as a `lagged` frame with a count; without it, legacy consumers retain
silent skip behavior. JSON output is NDJSON, one canonical frame per line.

This feed is ephemeral and has no replay or journal. After a lag or reconnect,
re-list with `sessions.list` and `worktrees.list` (or their CLI equivalents)
before relying on cached state. The command is read-only and never interacts
with `--allow-session-input`.

The local Unix socket keeps same-user authentication. A remote TCP client must
present the existing bearer token with read scope; filters cannot broaden that
authorization. If no daemon is running, the command reports the concise
recoverable no-daemon error.

`thegn serve` puts the same control API on TCP so a client on another
machine (or another window) can attach.

1. `thegn serve` starts the listener — `127.0.0.1:5380` by default.
2. `thegn pair new --scope read,write` mints a **single-use** pairing URL.
   The code is printed **once**; only its hash is stored.
3. The client redeems the URL for a token.

`thegn pair list` shows every pairing and its state; `thegn pair revoke
<id>` kills one (live streams drop on the next event).

> **v1 serves plaintext.** The control plane carries full PTY I/O, so the
> default bind is loopback. Put it behind a tailnet, VPN, or firewall
> before binding anything wider.

Local unix-socket peers of the same user get implicit admin, so local
verbs need no token; tokens are always required over TCP. Set
`[serve] require_approval = true` to make a redeemed token wait for
`thegn pair approve <id>` — otherwise possession of the pairing URL _is_
the credential.

## Lifetimes

`[daemon] lease_grace_secs` decides how long a session's PTY stays warm
after the last client detaches. The default is `0`, which means **never
reap** — a detached session lives until you close its pane, kill it, or
the machine restarts. That is what makes "come back tomorrow" work. A
non-zero value reaps on expiry instead.

`[daemon] idle_exit_secs` (default 30 minutes) exits the daemon once it
holds no sessions at all. A daemon started by `thegn serve` ignores it —
it keeps its listener up for clients that have not attached yet.

## Turning it off

`[daemon] enabled = false` runs every pane in-process, the way a plain
multiplexer would; `THEGN_NO_DAEMON=1` does it for a single run.

> Disabling the daemon **while daemon-backed panes are persisted stops
> those sessions** on the next launch: each pane respawns without the
> daemon and its orphaned copy is killed rather than left running
> invisibly.

See also [[configuration]] for where these keys live, and
[[workspaces-and-worktrees]] for how sessions map onto worktrees.
