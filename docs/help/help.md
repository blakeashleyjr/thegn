---
id: help
title: About this help
order: 50
contexts: [panel:help]
actions: [help]
---

# About this help

Everything here ships inside the binary — no network, no external docs.

## Using it

- `F1` opens help anywhere, at the page bound to whatever has focus
  (sidebar, a panel section, the center). In the [[sidebar]], `?` does
  the same.
- `Tab` switches between the contents tree and the page; `↑↓`/`j k`
  move and scroll; `PgUp/PgDn`, `g`/`G` for long pages.
- `n`/`p` cycle the page's links; `↵` follows one; `y` copies a link URL,
  and `[` and `]` are back/forward. External links open in the browser,
  with copying as fallback.
- `/` searches every page. Titles match fuzzily, bodies by substring;
  `↵` jumps to the matching section. Long contents show overflow markers.
- `Esc` closes.

## Where the content comes from

Pages are markdown files in the repo (`docs/help/`), embedded at build
time. Two pages are **generated at runtime** from their source tables:
[[keybindings]] reflects your effective keymap — rebinds included — and
[[config-reference]] is derived from the shipped example config.

[[keybindings]] is built from the same merged binding set `thegn keys
list` prints: the core registry, the host action table, and the keys each
zone handles itself.

Pages live in `docs/help/`; contributors see
`docs/extending/help-page.md`.
