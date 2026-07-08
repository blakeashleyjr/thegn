# Design

## Context to reuse

- **Preview pane** lives in `crates/superzej-host/src/` alongside the yazi file
  drawer. Text preview (AF 396) routes through tree-sitter highlighting; image
  preview (AF 399) routes raster output through the terminal graphics path
  (kitty / iTerm / sixel); hex view (AF 400) and file management (AF 606) are
  sibling preview/explorer behaviors.
- **Palette** lives in `crates/superzej-host/src/palette/` — a native iocraft
  TUI backed by nucleo and embedded ripgrep, with `PaletteMode` routing in
  `search_everywhere.rs`. Fuzzy file open is M 166.

## Rendering & event loop

Document viewers are new **content-type routes** on the existing preview seam,
not a new render substrate. When a file is previewed, its type selects a route:

- **Mermaid / PDF** — render to a raster image off the event loop (a background
  thread + channel + waker, the diff-watcher pattern), then hand the pixels to
  the **same graphics preview path as 399** (kitty / iTerm / sixel, picked by
  terminal capability detection). On terminals without graphics support, fall
  back to a text representation (Mermaid source / PDF text extraction) through
  the 396 text route.
- **CSV-table** — parsed off-loop and rendered as a bounded, scrollable table of
  cells inside the preview pane (pure text/glyph composition, no graphics).
- **Jupyter (.ipynb)** — parsed off-loop into ordered cells; code cells use the
  396 tree-sitter route, markdown cells render as text, and image outputs use
  the 399 graphics route.

All parsing/rasterization is off the loop; the preview pane only repaints on a
channel wake when the rendered result arrives, so an idle preview costs nothing.
The preview pane repaint stays a `Panes`/chrome repaint of its own rect and must
not force a full chrome recompose (render-plan invariant).

**Drag-drop** uses the existing TUI mouse seam: a press on a file row in the tree
begins a drag, and a release over a terminal pane inserts the file's path into
that pane's input, while a release over a markdown preview/editor inserts a
markdown link. This is TUI-limited (in-process panes only; no OS drag source)
and is a pure event-handling addition — no new render path.

**Quick-Open two-pass ranking** runs entirely inside the palette's existing
nucleo scoring. Pass one feeds tracked files (the git index / `ls-files`); pass
two appends gitignored / untracked files as a clearly-ordered second segment so
tracked matches always outrank gitignored ones for the same score. No new
matcher; the candidate set is split and concatenated before display.

## Persistence

**None.** Viewer routes, drag-drop, and two-pass ranking hold no durable state —
nothing new is written to the SQLite DB and no `user_version` bump is needed.
Existing per-worktree drawer state (file-explorer) is unchanged.

## Invariants

- **0% idle CPU.** All document parsing/rasterization happens off the loop on a
  background thread and is delivered over a channel with a `TerminalWaker` pulse;
  the loop never polls and never blocks on viewer work. The preview repaint stays
  a bounded pane/chrome repaint, never a forced full recompose.
- **AI-free.** None of these features depend on the AI/agent or LLM-proxy layers;
  they function fully with AI absent. AI remains strictly additive.
- **Graceful degradation.** Graphics-backed viewers fall back to text on
  terminals without kitty / iTerm / sixel support.
