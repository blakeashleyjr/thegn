# Tasks

## 1. Record the failure

- [x] 1.1 Split the waker-free `resolve_reorder` / `apply_reorder_drop` seam out
      of `perform_drop` so a drop's ordering logic is testable at all.
- [x] 1.2 Add the keystone tests — "a drop lands the source in the hovered row's
      slot" and "every slot of a run is reachable, including the tail" — driven over
      the full row-height matrix, and record the red baseline.

## 2. The rules

- [x] 2.1 `sidebar_order`: add the `displace` primitive, `Landing`, `place_at`,
      `displace_folder`, `workspace_order`, `displace_workspace`, `block_end`; make
      `locate` crate-visible; delete `drop_at` and `next_in_run`.
- [x] 2.2 `sidebar_mouse`: replace `spot_at`'s half-row math with the pure,
      y-free `spot_for_hover`; change `Spot::Reorder` to name a row's slot.

## 3. The gesture

- [x] 3.1 Freeze row heights for the life of a gesture (`SidebarLayoutLock`),
      re-stamped each loop iteration so a hydration cannot drop it.
- [x] 3.2 Re-derive the pressed row's band from its live placement.
- [x] 3.3 Proportional, capped edge autoscroll.
- [x] 3.4 Pointer capture (`should_forward_to_pane`) and `Esc` to cancel.
- [x] 3.5 Paint the insertion rule at the block end, and carry an off-screen drag
      source as `None` rather than `usize::MAX`.

## 4. Atomicity

- [x] 4.1 Extract `apply_workspace_order`; delete the step-walk; keep the
      attention-sort and pinned guards on both paths.

## 5. Verify

- [x] 5.1 Rewrite the two tests that encoded the broken model, and add the
      per-defect regressions.
- [x] 5.2 Mouse-path persistence round-trip and workspace-drop atomicity tests.
- [ ] 5.3 muse e2e drag spec (`test/muse/specs/33-sidebar-drag.yaml`): drive the
      gesture as raw SGR through `write:` (`\e[<0;C;RM` press, `\e[<32;C;RM` motion,
      `\e[<0;C;Rm` release — muse's `mouse` step has no motion action), covering a
      reorder, an Esc-cancel with a late inert release, and a drag past the bottom
      edge. Deferred: `just e2e` has ~30 pre-existing snapshot failures on this
      branch (a new statusbar `help` chip and a 3-column workspace-header indent),
      so new baselines cannot be recorded until that drift is resolved separately.
- [ ] 5.4 `just ci`. (`just test` — 4870 pass — and `just lint` are green.)
