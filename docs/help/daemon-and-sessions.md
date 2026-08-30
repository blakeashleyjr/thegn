---
id: daemon-and-sessions
title: Daemon & sessions
order: 13
actions: [detach, quit, quit-kill, fork-session]
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
| `thegn session fork <id>`                  | start a new daemon session from a live session; `--tab` places it in a new tab  |
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

`session fork` starts a fresh PTY from a live daemon session. It preserves the
launch recipe and reports the parent in `forked_from`; it never clones the
source process or screen. `--scrollback` gives the child a bounded,
owner-only handoff file, `--tab` adopts it in a new tab, and
`--fork-worktree` creates a separate worktree before launching. Exited sessions
must be reopened with `session open`; native agent conversations can also be
forked by supplying their recorded harness id.

## Moving a session to another profile

`thegn --profile <source> session move <worktree> --to-profile <target>` is an
admin-only host operation for moving one exact stored worktree path. The
target profile must already exist and is opened by path only; its config and
credential environment are never loaded into the source process. The git
worktree directory, branch, and git objects stay where they are.

The persisted worktree registration, every matching current group and tab,
sidebar collapse/pin keys, and the dispatch ledger (including notes and
artifact/chunk paths) are imported. Current layout groups/tabs are moved;
named global layouts, caches, active transports, and whole-session focus are
not. Opaque pane commands, scrollback, dispatch reports, and notes remain
unchanged in the target but are omitted from human and JSON audit output.
Credentials, tokens, identities, config overlays, accounts, pairings, and
secrets never cross the profile boundary.

The move is target-first: the target rows are committed and read back under a
sanitized fingerprint before exact source rows are deleted. Dispatch IDs are
fresh in the target, parent links are remapped only within the moved set, and
source daemon/pane IDs are cleared so the target compositor can create fresh
sessions. If a process stops after target confirmation, rerun the command;
the identical import is adopted and only pending source deletion is retried.
That partial result is retryable (exit 2).

The source daemon is optional for a cold move. If live sessions are found for
the exact path, or referenced by its persisted pane/dispatch rows, the move
refuses without `--kill`. With `--kill`, every live source ID is killed and a
second listing must show no survivors before either database is written.
`--dry-run` performs no kill and no database write, and `--json` emits a
redacted audit containing the selected group names, row counts, liveness,
commit/confirmation/deletion state, resume state, and notification status.
After confirmed cleanup, notification through a reachable daemon registered
in the target database is best effort; failure is reported as a warning.

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
