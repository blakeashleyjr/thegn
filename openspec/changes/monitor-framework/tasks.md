## 1. Spec and help (F0)

- [x] Write proposal, design, and additive system-monitor delta requirements
      for the framework contract (THE-829).
- [x] Register the change and THE-829 in the delivery ledger.
- [x] Update shipped system-monitor help and add per-tab help stubs.

## 2. Framework

Re-owned to THE-1362 (batch 72): THE-829 is Done; that issue ticks the delivered boxes below with evidence and owns the remaining cleanup tasks.

- [ ] F1a (THE-830): Implement shared numeric formatting in
      `thegn-core/src/numfmt.rs`.
- [ ] F1b (THE-831): Implement graph rasters in `thegn-core/src/viz_raster.rs`.
- [ ] F1c (THE-832): Add semantic colour and glyph roles in theme and termcaps.
- [ ] F2 (THE-833): Implement SeriesStore tiers, classes, and bounded storage.
- [ ] F3a (THE-834): Define the collector contract and create the
      `thegn-metrics/src/collect/**` skeleton.
- [ ] F3b: Add the registry, coverage hook, and reference loadavg, PSI, and
      uptime collectors.
- [ ] F4: Add the MonitorSampler worker: spawn on open, join on close, park
      while hidden or paused, use a three-frame pool, and provide
      `post_external`.
- [ ] F5a: Build the shared monitor kit basics.
- [ ] F5b: Build the shared monitor graph kit.
- [ ] F6a (THE-835): Implement the layout solver in `monitor/layout.rs`.
- [ ] F6b: Integrate the board, global keys, Escape layering, and `+N hidden`.
- [ ] F6c (THE-836): Mechanically carve monitor tabs into their modules.
- [ ] F7: Integrate monitor history.
- [ ] F8: Implement Overview and its help section.
- [ ] F9a: Add the test harness (`e2e_freeze`, snapshots, and fill-ratio).
- [ ] F9b: Add performance gates.

## 3. Shared readers

- [ ] SR-1: Implement the `/proc/stat` reader.
- [ ] SR-2: Implement cpufreq and CPU policy readers.
- [ ] SR-3: Implement hwmon enumeration and readers.
- [ ] SR-4: Implement cgroup readers and policy.
- [ ] SR-5: Implement the amdgpu `gpu_metrics` decoder and reader.
- [ ] SR-6 (THE-837): Implement process parsers and the process model.

## 4. Cleanup

- [ ] X-2: Remove monitor from masthead live sampling (`wants_live_stats`).
- [ ] X-3: Remove legacy `LegacyTab`, `sections.rs`, and dead `build.rs`
      helpers.
- [ ] SPEC-1: Sync the shipped contract and MODIFY "All system sampling runs
      off the event loop" to admit the `MonitorSampler`; archive this change when
      implementation is complete.
- [ ] Rewrite the help pause paragraph when F4 lands to document parked deep
      sampling and continuous masthead Class-A history.
- [ ] Run `just ci`.
