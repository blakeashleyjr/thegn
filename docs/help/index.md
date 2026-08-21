---
id: index
title: Welcome
order: 0
---

# thegn

A terminal-native git-worktree IDE that is its own terminal multiplexer.
One process, one session: each git repo is a **workspace**, each git
**worktree** is a **tab**, and the chrome — the [[sidebar]] tree, the
[[panel|diff/PR panel]], the tab bar, the status bar, and the pin strip —
is rendered in-process.

Press `F1` anywhere to open this help at the page for whatever has focus.
Press `/` here to search every page; `Tab` switches between the contents
list and the page; `↵` follows a link; `[` goes back.

## Start here

- [[getting-started]] — the mental model in two minutes
- [[workspaces-and-worktrees]] — the core objects and their lifecycle
- [[keybindings]] — every key, with _your_ rebinds applied
- [[best-practices]] — how thegn wants to be used

## The screen, by region

- [[sidebar]] — the left tree: workspaces, worktrees, terminals
- [[terminal-and-panes]] — the center: tabs, splits, tools
- [[panel]] — the right panel: diff, PRs, CI, the merge and PR queues, and more
- [[drawer-and-corner]] — the bottom file drawer and the corner pin
- [[bars]] — the masthead and the status bar

## Doing things

- [[command-palette]] — fuzzy-run any action, open anything
- [[search]] — the three search surfaces
- [[copy-and-select]] — selections, scrollback, registers
- [[git-and-diffs]] — the git tab, diffs, push/pull/rollback
- [[workflows]] — task-oriented guides: [[review-a-pr]],
  [[merge-queue]], [[sandboxing]]

## Beyond one machine

- [[daemon-and-sessions]] — detach, reattach, and serve thin clients
- [[share-and-forward]] — expose a port, forward a service
- [[cli]] — every command, for scripts and agents

## Everything else

- [[configuration]] — config layers and the [[config-reference]]
- [[terminal-compatibility]] — colors, glyphs, and `thegn doctor`
- [[release-channels]] — stable vs dev, and what's gated
- [[media]] — the now-playing controls
- [[help]] — how this help system itself works
