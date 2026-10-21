## Implementation

- [x] Add Overview as the first tab and preserve stable saved tab slugs.
- [x] Add the first board view, per-panel demand, and overlay-owned frame copy.
- [x] Exercise deterministic rendering at the three target sizes and color modes.
- [x] Capture and check in exact text/style goldens from the renderer fixture.
      Superseded, not performed as a separate check: the owner issue THE-871 was closed Done on 2026-10-18 after batch 46 landed the Overview tab with structural render tests at the three target sizes and colour levels (`overview_renders_all_target_sizes_and_terminal_levels`) instead of checked-in text/style goldens; the change is archived in batch 64.
- [x] Document the Overview tab and its controls.
