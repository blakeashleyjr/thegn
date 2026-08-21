---
id: panel
title: Diff / PR panel
order: 5
contexts:
  [
    zone:panel,
    panel:mine,
    panel:across,
    panel:issues,
    panel:problems,
    panel:jobs,
    panel:tests,
    panel:symbols,
    panel:notifications,
    panel:logs,
    panel:hosts,
    panel:telemetry,
  ]
actions: [focus-panel, toggle-panel]
---

# Diff / PR panel

The right panel tracks the focused worktree. `Alt-.` (or `Ctrl-→` from the
rightmost pane) focuses it; `Ctrl-Alt-p` hides it. It is a tabbed
accordion — three tabs, one open section at a time:

- **git** — changes (diff), commits, branches, stash, files
- **work** — your PRs, CI runs, the merge queue, the PR queue, issues,
  problems, jobs, tests, symbols
- **system** — notifications, logs, sandbox, hosts, environments, shares,
  port forwards, telemetry, media, keys

## Keys

- `Tab` / number keys — switch tabs / jump to a section
- `↑↓` / `j k` — move between sections; `↵` opens one
- `↵` again enters **row mode** (the cursor walks the section's rows);
  `Esc` steps back out
- `e` — cycle the width: normal → half → full-screen
- `?` — help for the open section (git-family sections show their own
  gitui cheatsheet)

Each section documents its own keys in the status bar while it has focus,
and [[keybindings]] lists them all under a `Panel · <section>` heading.

`[panel] sections` reorders the accordion or hides sections you never
use; the built-in order is the default.

## What each section shows

**git**

| Section    | Shows                          |
| ---------- | ------------------------------ |
| `changes`  | the working diff, hunk by hunk |
| `commits`  | branch history                 |
| `branches` | local and remote branches      |
| `stash`    | stash entries                  |
| `files`    | the worktree tree              |

**work**

| Section    | Shows                                                                |
| ---------- | -------------------------------------------------------------------- |
| `mine`     | one feed of everything assigned to you: issues, review requests, PRs |
| `across`   | failing CI across **all** worktrees, grouped by worktree (read-only) |
| `pr`       | PR state, CI check rollup, review decision for this branch           |
| `ci`       | run history and per-run state across providers                       |
| `merge`    | the local merge queue — per-branch land/defer status                 |
| `prq`      | the PR queue — queued pull requests on the forge and what blocks them |
| `issues`   | tracker issues                                                       |
| `problems` | compiler, linter, and test diagnostics                               |
| `jobs`     | configured shell jobs (build, test, run)                             |
| `tests`    | test results and the pass/fail rollup                                |
| `symbols`  | the LSP / tree-sitter outline for the selected file                  |

**system**

| Section         | Shows                                                     |
| --------------- | --------------------------------------------------------- |
| `notifications` | the notification list (see below)                         |
| `logs`          | thegn's own log stream                                    |
| `sandbox`       | live sandbox state for this worktree — see [[sandboxing]] |
| `hosts`         | configured `[host.*]` machines and their state            |
| `environments`  | configured `[env.<name>]` environments                    |
| `share`         | ports this worktree exposes — see [[share-and-forward]]   |
| `forward`       | auto port forwards to host loopback                       |
| `telemetry`     | live frame/loop counters for the running UI               |
| `media`         | now-playing and transport — see [[media]]                 |
| `keys`          | the effective keymap, same as [[keybindings]]             |

`hosts` and `environments` are **dev-channel only**; see
[[release-channels]].

The merge queue drives the local fold-actor, and the PR queue shepherds
pull requests on the forge — see [[merge-queue]] and [[pr-queue]] for
those workflows.

## Notifications

In row mode: `x` marks the row read, `d` deletes it, `A` shows read rows too,
`/` searches.

`a` is the **clear-all**, and it covers more than the list: as well as marking
every notification read it acknowledges the live "needs you" signals behind the
`✋` chip — failing CI, PR conflicts, changes requested — which are derived from
the PR/CI caches rather than from rows here. `g` toggles the scope between this
repo (the default) and every worktree. Both are described under [[bars]].
