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

| Command                                    | What it does                                                             |
| ------------------------------------------ | ------------------------------------------------------------------------ |
| `thegn session list`                       | every live session (`--json` for scripts)                                |
| `thegn attach [id]`                        | grab one interactively; `Ctrl-\` detaches                                |
| `thegn session attach --session <id>`      | stream its output to stdout                                              |
| `thegn session snapshot --session <id>`    | dump its current screen                                                  |
| `thegn session send --session <id> <text>` | type into it (`--enter` to run)                                          |
| `thegn session wait --session <id>`        | block until it's `exited`, `idle`, `blocked`, `done`, or `match:<regex>` |

`thegn attach` with no argument lists sessions to pick from. It is
local-only — it speaks the unix socket and never dials the TCP listener.

`session wait` is the one built for scripting: it exits 0 on match, 2 on
timeout, and 1 if there is no daemon, so a shell script can drive a long
build and block on it.

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
