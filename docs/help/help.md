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

[[keybindings]] is built from the same merged binding set `thegn keys
list` prints: the core registry, the host action table, and the keys each
zone handles itself. A test asserts every bindable action appears there,
so the page cannot fall behind the keymap again.

## For contributors

Every user-facing action must be claimed by a page's `actions:`
frontmatter — a ratchet test enforces it, so features can't ship
undocumented. A second ratchet requires the page to actually **mention**
what it claims, because claiming an id is cheap and was letting coverage
read 100% while the prose stood still.

Both allowlists may only shrink; `just help-ratchet-update` regenerates
them. See `docs/help/` and the `help` modules in `thegn-core` /
`thegn-host`.
