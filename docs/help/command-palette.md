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
    switch-identity,
    cycle-theme,
    connect-root,
    clone-open,
    new-environment,
    setup-wizard,
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
- **New environment…** — the add-environment wizard (`[env.<name>]`: cloud,
  ssh or local).
- **Setup wizard…** — re-run first-launch setup (forge auth, hosts, sandbox,
  appearance); the same as `thegn setup`.

All of these are ordinary actions — bind a chord to any of them with
`[keybinds]` (e.g. `connect-root = "Alt ."`).

- Account / bundle / profile / **identity** / font / theme switchers.
- **Switch identity** — pin a named per-tool identity (git config, git SSH
  key, `gh` config, GnuPG home, agent accounts) at the focused scope; each
  tool it sets overrides that credential for panes launched afterward, and
  tools it leaves unset fall through. The reusable, mix-and-match form of a
  profile's or bundle's `identity =`.
- **Switch profile** — launch/focus a whole-process profile. While the
  switcher is open, `Ctrl-Alt-↑/↓` **reorders** the highlighted profile; the
  order is saved to `~/.config/thegn/profiles-order.json` (shared across
  profiles) so it sticks.

Rows are ordered by frecency: what you use often and recently floats up.
Custom `[[actions]]` from your config appear here too — see
[[configuration]].
