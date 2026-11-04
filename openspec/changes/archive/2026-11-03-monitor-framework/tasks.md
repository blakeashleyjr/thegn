## 1. Spec and help (F0)

- [x] Write proposal, design, and additive system-monitor delta requirements
      for the framework contract (THE-829).
- [x] Register the change and THE-829 in the delivery ledger.
- [x] Update shipped system-monitor help and add per-tab help stubs.

## 2. Framework

Re-owned to THE-1362 (batch 72): THE-829 is Done; that issue ticks the delivered boxes below with evidence and owns the remaining cleanup tasks. Batch 83 ticked the delivered boxes with evidence; open boxes carry a `Gap:` line.

- [x] F1a (THE-830): Implement shared numeric formatting in
      `thegn-core/src/numfmt.rs`.
      Evidence: `thegn-core/src/numfmt.rs` —
      `numfmt::tests::golden_units_and_preferences`.
- [x] F1b (THE-831): Implement graph rasters in `thegn-core/src/viz_raster.rs`.
      Evidence: `thegn-core/src/viz_raster.rs` —
      `viz_raster::tests::full_and_basic_area_line_band_and_mirror_golden_rows`.
- [x] F1c (THE-832): Add semantic colour and glyph roles in theme and termcaps.
      Evidence: `thegn-core/src/theme.rs`, `termcaps.rs` —
      `theme::tests::hue_lookup_is_exhaustive_and_heat_clamps`,
      `termcaps::tests::glyph_token_covers_every_glyphset_field`.
- [x] F2 (THE-833): Implement SeriesStore tiers, classes, and bounded storage.
      Evidence: `thegn-core/src/series_store.rs` —
      `series_store::tests::tiers_windows_and_promote_demote`.
- [x] F3a (THE-834): Define the collector contract and create the
      `thegn-metrics/src/collect/**` skeleton.
      Evidence: `thegn-metrics/src/collect/mod.rs`, `registry.rs` —
      `collect::registry::tests::due_respects_mode_demand_phase_absent_and_retry_backoff`.
- [x] F3b: Add the registry, coverage hook, and reference loadavg, PSI, and
      uptime collectors.
      Evidence: `collect/registry.rs`, `coverage.rs`, `collect/linux/{loadavg,psi,uptime}.rs` —
      `collect::registry::tests::due_respects_mode_demand_phase_absent_and_retry_backoff`,
      `coverage::tests::collector_health_adds_coverage_without_downgrading_snapshot`,
      `collect::linux::loadavg::tests::held_file_updates_and_bad_data_is_parse_error`.
- [x] F4: Add the MonitorSampler worker: spawn on open, join on close, park
      while hidden or paused, use a three-frame pool, and provide
      `post_external`.
      Evidence: `monitor_sampler.rs`, `monitor_sampler_tests.rs`,
      `collect/frame.rs` — `monitor_sampler_tests::closed_does_not_spawn_and_open_reuses_one_named_worker`,
      `monitor_sampler_tests::hidden_and_paused_use_untimed_parks_then_open_samples`,
      `monitor_sampler_tests::close_closes_once_and_wakes_for_loop_owned_reap`,
      `monitor_sampler_tests::external_writes_are_latest_wins_while_parked`,
      `collect::frame::tests::pool_reuses_three_reset_allocations_and_drops_excess`.
- [x] F5a: Build the shared monitor kit basics.
      Evidence: `monitor/kit/`, `monitor/kit/tests.rs` —
      `monitor::kit::tests::widget_snapshot_matrix_degrades_for_every_unicode_and_color_level`.
- [x] F5b: Build the shared monitor graph kit.
      Evidence: `monitor/graphs/`, `monitor/graphs/tests.rs` —
      `monitor::graphs::tests::graph_glyph_and_style_goldens_follow_each_unicode_table`.
- [x] F6a (THE-835): Implement the layout solver in `monitor/layout.rs`.
      Evidence: `monitor/layout.rs` —
      `monitor::layout::tests::solve_is_deterministic_and_drops_by_priority_without_dropping_cpu`.
- [x] F6b: Integrate the board, global keys, Escape layering, and `+N hidden`.
      Evidence: `monitor/board_tests.rs`, `monitor_tests.rs` —
      `board_tests::zoom_uses_solver_area_and_escape_unzooms_before_close`,
      `board_tests::hidden_count_label_is_only_present_when_solver_drops_panels`,
      `monitor_tests::global_keys_stay_global_on_the_processes_tab`.
- [x] F6c (THE-836): Mechanically carve monitor tabs into their modules.
      Evidence: `monitor/tabs/` and per-tab view tests —
      `monitor::tabs::overview_view_tests::overview_renders_all_target_sizes_and_terminal_levels`.
- [x] F7: Integrate monitor history.
      Evidence: `monitor/history.rs`, `monitor/history_tests.rs` —
      `monitor::history_tests::psi_and_cpu_stat_parts_map_to_class_a_and_b_series`.
- [x] F8: Implement Overview and its help section.
      Evidence: `monitor/tabs/overview.rs`, `docs/help/system-monitor.md` —
      `monitor::tabs::overview_view_tests::overview_renders_all_target_sizes_and_terminal_levels`,
      `docs/help/system-monitor.md` ("## Overview");
      generic help gate: `help::ratchet_tests::registry_validates_cleanly`.
- [x] F9a: Add the test harness (`e2e_freeze`, snapshots, and fill-ratio).
      Evidence: `monitor/snapshot.rs`, `e2e_freeze.rs`, view snapshot tests —
      `monitor::snapshot::tests::fill_ratio_known_grids`,
      `e2e_freeze::tests::freeze_frame_is_stable_and_reuses_buffers`.
- [x] F9b: Add performance gates.
      Evidence: CPU and Overview `view_budget_tests.rs` —
      `monitor::tabs::cpu::view_budget_tests::cpu_usage_paint_stays_within_its_pinned_allocation_budget`,
      `monitor::tabs::overview::view_budget_tests::populated_overview_panels_stay_within_the_pinned_paint_budget`.

## 3. Shared readers

- [x] SR-1: Implement the `/proc/stat` reader.
      Evidence: `collect/parse/stat.rs`, `collect/linux/cpu_stat.rs` —
      `collect::linux::cpu_stat::tests::held_fd_reads_rewritten_file_and_marks_samples`.
- [x] SR-2: Implement cpufreq and CPU policy readers.
      Evidence: `collect/linux/cpufreq.rs`, `cpu_freq.rs` —
      `cpu_freq::tests::prefers_average_frequency_and_falls_back_to_scaling_frequency`.
- [x] SR-3: Implement hwmon enumeration and readers.
      Evidence: `collect/linux/hwmon.rs`, `collect/model/hwmon.rs`,
      `collect/policy/hwmon_group.rs` —
      `collect::linux::hwmon::tests::samples_stay_under_read_budget_and_close_releases_descriptors`.
- [x] SR-4: Implement cgroup readers and policy.
      Evidence: `collect/linux/cgroup.rs`, `collect/parse/cgroup.rs`,
      `collect/policy/cgroup_classify.rs` — `collect::linux::cgroup::tests::cgroup_root_targets_frame_and_absence_work`.
- [x] SR-5: Implement the amdgpu `gpu_metrics` decoder and reader.
      Evidence: `collect/parse/amd_gpu_metrics.rs`, `collect/linux/gpu_amd.rs` —
      `collect::linux::gpu_amd_tests::apu_sampling_stays_under_ten_preads_and_reads_totals_only_at_discovery`.
- [x] SR-6 (THE-837): Implement process parsers and the process model.
      Evidence: `procs_parse.rs`, `procs_model.rs`, `procs_linux.rs` —
      `procs_parse::tests::stat_handles_comm_delimiters_flags_and_old_kernels`,
      `procs_linux_tests::steady_pass_under_budget_and_does_not_grow_entry_table`.

## 4. Cleanup

- [x] X-2: Remove monitor from masthead live sampling (`wants_live_stats`).
      Superseded by the owner decision of 2026-10-19 ("X-2 masthead sampling: keep 1 Hz."):
      `monitor::wants_live_stats` (monitor.rs:1069) is kept; nothing is removed.
- [x] X-3: Remove legacy `LegacyTab`, `sections.rs`, and dead `build.rs`
      helpers.
      Superseded as written: `monitor/sections.rs` is gone, `monitor/build.rs`
      carries no dead helper (`dead_code` is denied), and `LegacyTab` was the
      live renderer of Processes, Notifications, and engine-less GPU, so it was
      renamed `SectionStackTab` in `monitor/board.rs` instead of removed.
      Per-tab board views for those tabs are not part of this change.
- [x] SPEC-1: Sync the shipped contract and MODIFY "All system sampling runs
      off the event loop" to admit the `MonitorSampler`; archive this change when
      implementation is complete.
      Synced: merged the shipped framework requirements, retained the existing
      Overview landing contract, and documented `MonitorSampler`; archived in
      batch 85.
- [x] Rewrite the help pause paragraph when F4 lands to document parked deep
      sampling and continuous masthead Class-A history.
      Evidence: `docs/help/system-monitor.md` (the `Space` pauses paragraph);
      `monitor_sampler_tests::hidden_and_paused_use_untimed_parks_then_open_samples`.
- [x] Run `just ci`.
      Superseded, not performed as a separate check: batch 85's single final
      gate (`just ci`-equivalent stamped gate) covers it; the change is archived
      in batch 85.
