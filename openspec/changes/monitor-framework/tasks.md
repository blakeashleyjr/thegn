## 1. Spec and help (F0)

- [x] Write proposal, design, and additive system-monitor delta requirements
      for the framework contract (THE-829).
- [x] Register the change and THE-829 in the delivery ledger.
- [x] Update shipped system-monitor help and add per-tab help stubs.

## 2. Framework

- [ ] F1a (THE-830): Implement the monitor board and shared frame lifecycle.
- [ ] F1b (THE-831): Add panel layout, focus, downgrade, and hidden-panel
      accounting.
- [ ] F1c (THE-832): Add panel source health and absence handling.
- [ ] F2 (THE-833): Implement the deep sampler lifecycle and frame publication.
- [ ] F3a (THE-834): Implement bounded tiered history storage.
- [ ] F3b: Integrate history tiers and honest gaps with monitor rendering.
- [ ] F4: Park deep sampling while hidden or paused.
- [ ] F5a: Establish the monitor frame perf workload and budgets.
- [ ] F5b: Compute and blit graph rasters within the repaint pipeline.
- [ ] F6a (THE-835): Implement Overview as the first tab and persist tab slugs.
- [ ] F6b: Unify monitor key dispatch and Escape layering.
- [ ] F6c (THE-836): Align Containers actions with the unified key scheme.
- [ ] F7: Implement monitor colour and glyph roles.
- [ ] F8: Implement Overview content and its help section.
- [ ] F9a: Add remaining shared framework integration.
- [ ] F9b: Verify platform behaviour and integration boundaries.

## 3. Shared readers

- [ ] SR-1: Integrate shared CPU and memory readers.
- [ ] SR-2: Integrate thermal and power readers.
- [ ] SR-3: Integrate network readers.
- [ ] SR-4: Integrate disk readers.
- [ ] SR-5: Integrate GPU readers.
- [ ] SR-6 (THE-837): Reconcile shared-reader ownership and lifecycle.

## 4. Cleanup

- [ ] X-2: Remove superseded framework paths and reconcile help after F4.
- [ ] X-3: Reconcile programme tracking and archive completed implementation
      work.
- [ ] SPEC-1: Sync the shipped contract and MODIFY "All system sampling runs
      off the event loop" to admit the `MonitorSampler`; archive this change when
      implementation is complete.
- [ ] Run `just ci`.
