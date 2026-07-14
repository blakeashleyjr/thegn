---
id: command-palette
title: Command palette
order: 8
actions:
  [
    palette,
    switch-font,
    switch-account,
    switch-bundle,
    switch-profile,
    cycle-theme,
  ]
---

# Command palette

`Ctrl-Space` opens a fuzzy palette of **every action** — it is the
complete, always-current action reference (each row shows its effective
chord). Type to filter, `↵` runs, `Esc` closes, `Tab` cycles modes.

## Modes (type the prefix, or Tab)

- _(none)_ — all actions
- `~` — **frecency opener**: workspaces + worktrees ranked by how often
  and how recently you use them; `↵` lands in that worktree's tab
- `>` — files in the focused worktree
- `/` — content search across files
- `@` — git: branches, commits
- `#` — symbols
- `!` — tasks · `$` — problems · `%` — tests

## Notable palette-only actions

- **Connect to root** — jump from a shell nested deep in a subdirectory
  straight to the owning worktree's tab.
- **Clone and open** — paste a git URL; it clones off-loop and opens as a
  workspace.
- Account / bundle / profile / font / theme switchers.

Rows are ordered by frecency: what you use often and recently floats up.
Custom `[[actions]]` from your config appear here too — see
[[configuration]].
