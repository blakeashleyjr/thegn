---
id: search
title: Search
order: 9
actions: [search-pane, search-global, palette-files, palette-content]
---

# Search

Four search surfaces, one habit: type, arrow, `↵`.

| Key               | Searches                                  |
| ----------------- | ----------------------------------------- |
| `Ctrl-Alt-/`      | the **focused pane's** printed output     |
| `Ctrl-/`          | every pane in the **active worktree**     |
| palette, `/` mode | **file contents** in the focused worktree |
| palette, `>` mode | **file names** — fuzzy-open a file        |

## Terminal search

`Ctrl-Alt-/` and `Ctrl-/` are incremental: results narrow as you type,
`↑↓` walks matches, `↵` jumps to one. Cross-pane search labels each hit
with the pane it came from, so it doubles as "which of these six
terminals printed that error".

Searching never stalls typing or redrawing, even at workspace scope over
hundreds of thousands of lines: matching runs on a background thread and the
list says `searching…` until the ranked results are in. Pressing `↵` in the
meantime jumps to the best hit as soon as it is found.

Each hit is labelled **worktree · tab · program**, followed by how long ago the
line was captured (`now`, `3m`, `2d`), so two panes running the same program in
different tabs are told apart and an error from three days ago does not look
like one from three minutes ago. The panel under the list previews the matched
line with the two lines either side of it, the match highlighted, so you can
recognise the context before you jump. Opening the overlay with an
empty query lists the pane's most recent lines instead of an empty box.
Identical lines from one pane are grouped into a single row labelled with the
repeat count, and `↵` goes to the newest of them.

`↵` scrolls the pane to the exact grid line when the terminal emulator still
holds it and highlights the match. The mark stays until the next key or a
scroll. `n` and `N` step to the next and previous hit without reopening the
overlay; the status line shows which match you are on. When the line has
already scrolled out of the emulator's grid, or came from a full-screen app,
the status line says so and points you to replay (`Alt r`), which has the full
history, instead of scrolling to an unrelated line.

Matching is **literal** by default: the query is a substring, case-insensitive
unless it contains an uppercase letter (smart case). Hits are ranked by match
quality, then most recent first. These keys work in the overlay; the footer
lists them:

| Key      | Effect                                              |
| -------- | --------------------------------------------------- |
| `Alt+c`  | toggle exact case (turns smart case off)            |
| `Alt+w`  | toggle whole-word matching                          |
| `Alt+r`  | toggle regex matching (as in [[search-replace]])    |
| `Alt+f`  | toggle fuzzy matching and back                      |
| `Ctrl+r` | cycle through your recent queries from this session |

Recent queries are kept in memory only, so they are gone after a restart.
The active controls show as `Aa`, `ab`, `.*` and `~` chips beside the scope
in the overlay header.

`Ctrl-/` starts at the active worktree. `Tab` widens the scope and `Shift-Tab`
narrows it, through pane, tab, worktree and workspace. Workspace covers every
worktree in the active workspace; there is no wider scope. These scopes are the
presets for how far a search reaches.

The source is the output buffer thegn keeps for each pane: a bounded in-memory
ring of the lines the pane printed. It is not your shell history, and it is not
the terminal's own scrollback. A pane with no output yet shows "no output yet".

`[search] max_results` caps how many hits are collected — raise it for a
big scrollback, lower it if searching feels heavy. When the cap is reached
the footer shows `N of M+ · too many results` — the count is a lower bound, so
narrow the query or scope. See [[configuration]].

## File search

The [[command-palette]] handles files. `/` greps **contents** in the
focused worktree; `>` fuzzy-matches **paths**. Both open what you pick in
the drawer or an editor pane.

The IDE presets map their file keys onto those modes. With `keymap_preset =
"vscode"`, `Ctrl-P` opens the palette in `>` mode (go to file), and
`Ctrl-Shift-F` opens it in `/` mode (find in path). With `"jetbrains"`,
`Ctrl-Shift-F` is the same find in path, and `Ctrl-E` opens the palette's
recent files. The file picker only lists paths once you type a query.

Content search matches the query as literal text, ignoring case unless the
query contains an uppercase letter. It honours `.gitignore`, includes dotfiles,
and skips `.git`, symlinks, binary files, and files over 2 MiB.

This is the one people mix up: `Ctrl-/` searches what the active worktree's
terminals _printed_, the palette's `/` searches what is _on disk_.

## Searching this help

`/` inside the help overlay searches every page. Titles match fuzzily and
bodies by substring; each hit shows the line it matched, and `↵` jumps
straight to that section. A page mentioning your term several times gives
you one result per mention, so you land on the right one. See [[help]].

## Related

- [[copy-and-select]] — scrollback, selections, and registers
- [[panel]]'s **work → attention** section is search-adjacent: it surfaces
  failing CI across every worktree without you asking
