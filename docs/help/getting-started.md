---
id: getting-started
title: Getting started
order: 1
---

# Getting started

thegn is **one session**. Switching repos or worktrees is a tab switch,
never a session change — quitting and relaunching restores your exact
project, worktree, and pane position.

## First-run checklist

1. Install git, then start `thegn`. The flake and `./install.sh` also install
   `tg`; the release tarball contains only `thegn`.
2. Run `thegn doctor` and review the detected capabilities, channel, and
   sandbox backend.
3. If `Alt` chords do nothing, check your terminal setup; on macOS, configure
   Option as Alt. See [[terminal-compatibility]].
4. Press `Alt-W` to open or clone a repository, then `Alt-w` to create a
   worktree and choose what to run.
5. Press `Ctrl-Space` to open the [[command-palette]], or `F1` for help.
6. Run `thegn config edit` when you want to change a default.

## The mental model

- A **project** is a repo. Create or open one with `Alt-W`.
- A **worktree** is a git worktree, shown as a tab. `Alt-w` branches one
  off the base branch and asks what to run in it — a coding agent, a tool,
  or a plain shell, optionally sandboxed.
- A **pane** is a terminal split inside a tab: `Alt-p` smart-splits,
  `Alt-n` splits down, `Alt-N` splits right.

## Moving around

- `Ctrl-←/↓/↑/→` (or `Ctrl-h/j/k/l`) moves focus across one spatial map:
  [[sidebar]] ← center panes → [[panel]], masthead above, status bar below.
- `Alt-←/↓/↑/→` moves focus the same way, but running off the outer edge
  falls through to the next thing: `←/→` to the previous/next tab, `↑/↓` to
  the previous/next worktree — one motion for the whole layout.
- `Shift-Alt-↑/↓` cycles projects, stepping past any you have collapsed.
- `Alt-1..9` jumps to a worktree, `Ctrl-1..9` to a project, by sidebar
  order. Some terminals cannot send `Ctrl-<digit>` at all; if those do
  nothing, see [[terminal-compatibility]].
- `Ctrl-Space` opens the [[command-palette]] — every action, fuzzy-searched.

## Starting outside a git repository

On a first run with no saved workspace, the launch directory becomes a project;
if it is not a git repository, it is shown as a plain-directory project marked
`⌂`. You get a shell there. `Alt-w` shows
`new-worktree: not inside a git repository` and creates nothing; use `Alt-W` to
open or clone a repository. Later launches
reopen the last workspace, regardless of the launch directory.

## Closing things

`Alt-x` is one smart **close**: the focused pane if the tab is split,
otherwise the tab. `Alt-X` (Shift) escalates to removing the whole worktree
and its tab — the branch is kept. Close never deletes a worktree unless you
reach for the Shift variant.

## Configuration and where things live

The release tarball and Nix package do not create a config file; thegn runs on
built-in defaults until you configure it. Use `thegn config path` to see its
location, `thegn config edit` to create or change it, and `thegn config validate`
to check it. Every key is listed in [[config-reference]]; [[configuration]]
explains how settings are layered.

| What             | Where                                                                           |
| ---------------- | ------------------------------------------------------------------------------- |
| Config file      | `$XDG_CONFIG_HOME/thegn/config.toml` (default: `~/.config/thegn/config.toml`)   |
| Per-repo overlay | `.thegn.toml` in the repository root                                            |
| State database   | `$XDG_STATE_HOME/thegn/thegn.db` (default: `~/.local/state/thegn/thegn.db`)     |
| Crash reports    | `$XDG_STATE_HOME/thegn/crash` (default: `~/.local/state/thegn/crash`)           |
| Log file         | `$XDG_STATE_HOME/thegn/logs/thegn.log` (`[log] file = true` or `THEGN_LOG` set) |
| New worktrees    | `~/.thegn/worktrees/<repo>/<label>--<digest>` by default                        |

By default, worktree panes run in a sandbox. The `standard` profile makes files
outside the worktree read-only, while your home directory and its credentials
remain readable; `sealed` hides `$HOME`. Native plugins run unsandboxed. If no
sandbox backend is usable, the launch is refused; set `[sandbox] enabled = false`
to opt out. See [[sandboxing]].

## When you're stuck

- The bottom status bar always shows the keys that work _right now_.
- `F1` opens help for whatever has focus; `/` searches all of it.
- Every chord thegn does not bind passes through to the pane with its
  modifiers intact, whether or not the keybind lock is active.
- `Ctrl-g` toggles the keybind lock, letting a TUI use chords thegn owns.
- `thegn doctor` reports detected terminal capabilities and the sandbox backend.
- `thegn doctor bundle --out FILE` writes a redacted support archive; review it
  before sharing. See [[debugging]].
