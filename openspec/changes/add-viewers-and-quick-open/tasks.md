# Tasks

## 1. Document viewers (AF 775)

- [x] 1.1 Content-type detection that maps a previewed file to a route
      (Mermaid / PDF / CSV / Jupyter / text / image) — **unit tests** for the
      pure extension/sniff → route mapping, including ambiguous/unknown fallback.
      (`thegn_core::preview::route_for`.)
- [x] 1.2 CSV → bounded scrollable table model (pure parse + cell layout) —
      **unit tests** for parsing, column sizing, and row/column bounds.
      (`thegn_core::preview::CsvTable`; rendered by `preview_render::csv_lines`.)
- [x] 1.3 Jupyter `.ipynb` → ordered cell model (code via 396, markdown text,
      image outputs via 399) — **unit tests** for cell ordering and cell-type
      classification. (`thegn_core::preview::Notebook`;
      rendered by `preview_render::notebook_lines`.)
- [x] 1.4 Off-loop Mermaid / PDF / image rasterization handed to the graphics
      path (kitty; `src/rasterize.rs` → `src/graphics.rs` → `src/preview_gfx.rs`)
      with text fallback (Mermaid source / PDF `pdftotext`) when graphics are
      unsupported; delivered over a channel + waker (no loop blocking).
      Note: kitty is the implemented graphics protocol (iTerm/sixel degrade to
      text); pixels need a live kitty/ghostty/wezterm terminal to confirm.

## 2. Drag-drop affordances (AF 776)

- [x] 2.1 Mouse-seam drag model: press-on-tree-row → drag, release-over-target →
      action (terminal path-insert vs markdown-link insert) — **unit tests** for
      the pure target-resolution + payload-formatting logic. (`src/dragdrop.rs`.)
- [ ] 2.2 Wire the drag model into the existing TUI mouse event handling
      (in-process panes only; TUI-limited). **Deferred** to a focused follow-up:
      the pure model is complete + tested, but the panel-row-hit-test →
      pane-target integration is a delicate `run.rs`/panel change that needs a
      live TUI to verify. `dragdrop.rs` is `#![allow(dead_code)]` until wired.

## 3. Quick-Open two-pass ranking (M 777)

- [x] 3.1 Split the candidate set into tracked (pass one) and gitignored/
      untracked (pass two) and concatenate so tracked outranks gitignored at
      equal score — **unit tests** for ordering, equal-score tie-break, and the
      empty-second-pass case. (`fff_backend::merge_two_pass`.)
- [x] 3.2 Wire the two-pass candidate ordering into the palette's fuzzy file
      open mode. (`fff_backend::file_search` now does both passes; the palette
      already calls it via `search_everywhere::spawn_file_search`.)

## Validate

- [ ] Run `just ci`
