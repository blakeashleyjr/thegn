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
- **work** — your PRs, CI runs, the merge queue, issues, problems, jobs,
  tests, symbols
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

Each section documents its own keys in the status bar while it has focus.
The PR view shows state, CI check rollup, and review decision for the
branch's PR; the merge queue drives the local fold-actor. See
[[merge-queue]] for the queue workflow.

## Notifications

In row mode: `x` marks the row read, `d` deletes it, `A` shows read rows too,
`/` searches.

`a` is the **clear-all**, and it covers more than the list: as well as marking
every notification read it acknowledges the live "needs you" signals behind the
`✋` chip — failing CI, PR conflicts, changes requested — which are derived from
the PR/CI caches rather than from rows here. `g` toggles the scope between this
repo (the default) and every worktree. Both are described under [[bars]].
