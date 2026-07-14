---
id: help
title: About this help
order: 50
actions: [help]
---

# About this help

Everything here ships inside the binary — no network, no external docs.

## Using it

- `F1` opens help anywhere, at the page bound to whatever has focus
  (sidebar, a panel section, the center). `?` does the same in
  non-typing zones like the [[sidebar]].
- `Tab` switches between the contents tree and the page; `↑↓`/`j k`
  move and scroll; `PgUp/PgDn`, `g`/`G` for long pages.
- `n`/`p` cycle the page's links; `↵` follows one; `[` and `]` are
  back/forward.
- `/` searches every page. Titles match fuzzily, bodies by substring;
  `↵` jumps to the matching section.
- `Esc` closes.

## Where the content comes from

Pages are markdown files in the repo (`docs/help/`), embedded at build
time. Two pages are **generated at runtime** and can never drift:
[[keybindings]] reflects your actual effective keymap — rebinds included —
and [[config-reference]] is derived from the shipped example config.

## For contributors

Every user-facing action must be claimed by a page's `actions:`
frontmatter — a ratchet test enforces it, so features can't ship
undocumented. See `docs/help/` and the `help` modules in `thegn-core` /
`thegn-host`.
