---
id: search
title: Search
order: 9
actions: [search-pane, search-global]
---

# Search

Four search surfaces, one habit: type, arrow, `↵`.

| Key               | Searches                                      |
| ----------------- | --------------------------------------------- |
| `Ctrl-Alt-/`      | the **focused pane's** scrollback and history |
| `Ctrl-/`          | **every pane** at once                        |
| palette, `/` mode | **file contents** in the focused worktree     |
| palette, `>` mode | **file names** — fuzzy-open a file            |

## Terminal search

`Ctrl-Alt-/` and `Ctrl-/` are incremental: results narrow as you type,
`↑↓` walks matches, `↵` jumps to one. Cross-pane search labels each hit
with the pane it came from, so it doubles as "which of these six
terminals printed that error".

`[search] max_results` caps how many hits are collected — raise it for a
big scrollback, lower it if searching feels heavy. See
[[configuration]].

## File search

The [[command-palette]] handles files. `/` greps **contents** in the
focused worktree; `>` fuzzy-matches **paths**. Both open what you pick in
the drawer or an editor pane.

This is the one people mix up: `Ctrl-/` searches what your terminals
_printed_, the palette's `/` searches what is _on disk_.

## Searching this help

`/` inside the help overlay searches every page. Titles match fuzzily and
bodies by substring; each hit shows the line it matched, and `↵` jumps
straight to that section. A page mentioning your term several times gives
you one result per mention, so you land on the right one. See [[help]].

## Related

- [[copy-and-select]] — scrollback, selections, and registers
- [[panel]]'s **work → across** section is search-adjacent: it surfaces
  failing CI across every worktree without you asking
