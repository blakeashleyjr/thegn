---
id: best-practices
title: Best practices
order: 40
---

# Best practices

Opinionated guidance — how thegn wants to be used.

## One worktree per task

Worktrees are cheap and isolated; branch switching in place is not.
`Alt-w` for every task — a feature, a review, an experiment — and `Alt-X`
when it lands. Your `main` checkout stays clean and a broken experiment
never touches your other work.

## Let the queue land your work

Instead of switching to `main` to merge, queue finished branches
([[merge-queue]]) and drain. The fold-actor gates every landing, handles
the ref advance atomically, and never needs `main` checked out.

## Sandbox your agents

A coding agent with shell access deserves a container. Configure
`[sandbox]` once ([[sandboxing]]); the worktree stays on the host so
diffs and git state keep working, while the agent's blast radius is the
container.

## Keep hands on the keyboard

- The status bar always shows what works _right now_.
- `Ctrl-Space` runs anything; `~` jumps anywhere, ranked by frecency.
- Learn the modifier grammar and chords become guessable: **Ctrl** moves
  focus, **Alt** creates objects and opens tools, **Alt-Shift** is one
  level up, **Ctrl-Alt** toggles chrome.

## Housekeeping

- `thegn disk` shows per-worktree disk usage; `thegn clean` reclaims
  `target/`-style build dirs.
- Prune stale worktrees from the [[sidebar]] delete menu — deleting from
  disk is always the explicit second choice.
- `attention` sort (`[ui] sidebar_workspace_sort`) bubbles what needs you.
