# Add document viewers and two-pass Quick-Open

## Summary

Three small viewer + palette wins, all AI-free:

- **Document viewers** — render Mermaid, PDF, CSV-as-table, and Jupyter notebooks
  in the preview pane, extending the existing tree-sitter text preview (396) and
  graphics image preview (399).
- **Drag-drop affordances** — through the existing TUI mouse seam, allow
  dragging a file from the tree onto a terminal pane (insert its path) or onto a
  markdown preview/editor (insert a link), within TUI limits.
- **Quick-Open two-pass ranking** — refine fuzzy file open (166) so tracked
  files rank in a first pass and gitignored files surface in a second pass,
  rather than being interleaved or omitted.

## Impact

- **AF** (file viewer / search) — items **775** (document viewers) and **776**
  (drag-drop affordances); extends **396** (preview pane / tree-sitter), **399**
  (image preview / kitty / iTerm / sixel), and relates to **400** (hex view) and
  **606** (file management).
- **M** (command palette) — item **777** (Quick-Open two-pass ranking); refines
  **166** (fuzzy file open across workspace).
- Touches two existing capabilities: `file-explorer` (775, 776) and
  `command-palette` (777). No new capability is created.

## Rationale

The preview pane already routes text through tree-sitter (396) and images
through the graphics path (399); document viewers slot in as additional
content-type routes on the same seam — no new rendering substrate. Drag-drop is
purely an additive use of the existing mouse seam. Two-pass ranking is a small,
deterministic refinement of the nucleo-backed Quick-Open list that makes the
common case (tracked files) win without losing access to gitignored files.

## Non-goals

- No editing of PDF / Jupyter / CSV documents — viewers are read-only previews.
- No AI/agent dependency anywhere; all features work with the AI layer absent.
- No new graphics protocol work beyond reusing kitty / iTerm / sixel (399).
- No general OS-level drag-and-drop; affordances are limited to the in-process
  TUI mouse seam.
- No persistence of viewer or ranking state (see design.md).
