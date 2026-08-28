# Tasks — fix-drag-grab-precision

## 1. Pure geometry (chunk 1)

- [x] 1.1 `drag_hit.rs`: `SepSide`, `sep_grab`, `sep_is_exact`, `sep_follow`,
      unit tested at the module, wired behind `#[expect(dead_code)]` until
      chunk 2 calls them.
- [x] 1.2 Tab-chip gap hit in `chrome.rs` (`chrome_tests.rs` keeps the
      expansion assertion green).

## 2. Loop wiring (chunk 2)

- [x] 2.1 `border_at` takes `slop: usize`; the call site passes
      `crate::center::PANE_HPAD`; tests cover slop 0 (existing assertions kept,
      threaded `0`) and slop > 0 on both axes, plus content-never-stolen at
      slop 3.
- [x] 2.2 Separator grab state `(press_x, sep, moved, width_snapshot)` for
      both separators; grab on the two-column band, extra cell gated on
      `hit_pane.is_none()`; the press mutates nothing.
- [x] 2.3 Motion: threshold at the press column, Wide drop-out on the first
      moved sample, `sep_follow` width through the unchanged clamps.
- [x] 2.4 Release: moved → persist + report as before; motionless → no-op
      (no persist, no width report).
- [x] 2.5 The existing Esc-cancel arm also cancels a separator grab,
      restoring the snapshotted width and persisting nothing.
- [x] 2.6 `pane_lift` carries `(pane, press_x, press_y, moved)`; a motionless
      release focuses the lifted pane; a moved release swaps / re-anchors as
      before.
- [x] 2.7 Help prose: `docs/help/sidebar.md` (row-drag drop target + width
      drag), `docs/help/panel.md` (grab band, click, Esc),
      `docs/help/terminal-and-panes.md` (frame click focuses, drag
      rearranges, Esc cancels).

## 3. Sidebar drop target (chunk 3)

- [x] 3.1 `sidebar_view.rs` row-at clamp + `handlers/sidebar_mouse.rs` drop
      resolution: release inside the sidebar's rect lands on the nearest row,
      outside cancels.

## 4. Validation

- [x] 4.1 Scoped: `just quick thegn-host`; `cargo nextest run -p thegn-host
pane_drag` / `border_at` / `drop_on`.
- [x] 4.2 `just openspec-validate` (`openspec validate --all --strict`).
- [ ] 4.3 Pre-PR gate, run **once** when the branch is complete:
      `just ci`. Note: this change alters frames, so `just e2e` baselines are
      stale by construction — re-recording is a separate, deliberate step
      (`just e2e-update`), not part of the fix.
