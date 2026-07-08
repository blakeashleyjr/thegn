# Tasks

## 1. Document viewers (AF 775)

- [ ] 1.1 Content-type detection that maps a previewed file to a route
      (Mermaid / PDF / CSV / Jupyter / text / image) — **unit tests** for the
      pure extension/sniff → route mapping, including ambiguous/unknown fallback.
- [ ] 1.2 CSV → bounded scrollable table model (pure parse + cell layout) —
      **unit tests** for parsing, column sizing, and row/column bounds.
- [ ] 1.3 Jupyter `.ipynb` → ordered cell model (code via 396, markdown text,
      image outputs via 399) — **unit tests** for cell ordering and cell-type
      classification.
- [ ] 1.4 Off-loop Mermaid / PDF rasterization handed to the 399 graphics path
      (kitty / iTerm / sixel) with text fallback when graphics are unsupported;
      delivered over a channel + waker (no loop blocking).

## 2. Drag-drop affordances (AF 776)

- [ ] 2.1 Mouse-seam drag model: press-on-tree-row → drag, release-over-target →
      action (terminal path-insert vs markdown-link insert) — **unit tests** for
      the pure target-resolution + payload-formatting logic.
- [ ] 2.2 Wire the drag model into the existing TUI mouse event handling
      (in-process panes only; TUI-limited).

## 3. Quick-Open two-pass ranking (M 777)

- [ ] 3.1 Split the candidate set into tracked (pass one) and gitignored/
      untracked (pass two) and concatenate so tracked outranks gitignored at
      equal score — **unit tests** for ordering, equal-score tie-break, and the
      empty-second-pass case.
- [ ] 3.2 Wire the two-pass candidate ordering into the palette's fuzzy file
      open mode (`palette/`, `search_everywhere.rs`).

## Validate

- [ ] Run `just ci`
