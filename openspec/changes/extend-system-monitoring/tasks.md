# Tasks — extend-system-monitoring

## 1. Pin the existing contract (verify + test, no new behaviour)

- [x] 1.1 Swept the sampling paths against the spec: `StatsSampler` on the
      refresh ticker, `ProcSampler` on its own thread, both `Qos::Background`,
      channel + waker delivery — verified against `hydrate.rs` (both samplers
      declare Background QoS) and the existing `sample_is_well_formed` /
      monitor-gating tests. `spec.md` codifies the contract.
- [x] 1.2 Absent-metric rule already end-to-end (widget hide via
      `masthead_widget`, tab hide via `MonitorTab::present`, inert alert via
      `resource_alert` Rule 4); the alert-never-fires-on-absent clause is
      covered by `a_metric_the_platform_lacks_never_fires` +
      `an_absent_reading_never_clears_a_standing_alert` (existing) and the new
      `disk_eta_*` tests.

## 2. Doctor coverage report

- [x] 2.1 `thegn-metrics::coverage` — pure `coverage(&StatsSnapshot) ->
Vec<FamilyReport>` (available / absent{NotOnThisOs, NoHardware,
      NoPermission-reserved}); unit-tested (`coverage.rs` tests).
- [x] 2.2 `cmd/doctor.rs` prints the block + `system_metrics` JSON; the
      two-sample warmup is CLI-only (never on the compositor fast path).

## 3. Processes tab

- [x] 3.1 Overlay filter state (`/`, incremental, `Esc` clears without closing);
      pure `procs_view::rows` filter + tests, overlay confirm/filter tests.
- [x] 3.2 Tree grouping toggle from sampled ppid chains, nearest-kept-ancestor
      elision, cycle-safe; pure + tested in `procs_view`.
- [x] 3.3 Signal action: `x` confirm (pid, name, owner), SIGTERM then
      SIGKILL-on-second-x, error surfaced; `platform::signal_pid` seam (unix
      SIGTERM/SIGKILL with ESRCH/EPERM classification + pid guard, Windows
      `TerminateProcess`). **Deviation:** kept it an inline overlay key (no
      `ACTION_SPECS` id), matching the existing monitor keys (c/m/n/g/s/…) — an
      `ACTION_SPECS` id is palette-exposed, which would contradict the
      TUI-only/no-external-door requirement. Documented in the help prose instead.
- [x] 3.4 Updated `docs/help/system-monitor.md` (filter/tree/signal + Disk lane).

## 4. Disk tab worktree lane

- [x] 4.1 `worktree_disk_rows` feeds the `[disk]` scanner cache (size, `target/`
      share, age from a new `SidebarStatus.disk_stamps`) into the Disk tab;
      sorted list; per-row `x` clean wired off-loop (`monitor::spawn_clean`).
- [x] 4.2 Days-to-full projection: `thegn_core::disk_fill::project` (pure
      least-squares fit + honesty gates, unit-tested) over a new
      `Metric::DiskFree` ring; rendered in the worktrees-filesystem heading.
- [x] 4.3 `[stats.alerts] disk_eta` (default 0/off) via new
      `AlertMetric::DiskEta` through `resource_alert`; config example + tests.

## 5. Command collectors

- [x] 5.1 `MetricsTarget` gains `kind` (`MetricsTargetKind`) + `command` argv;
      `command_argv()` validator; malformed targets dropped in `post_process`;
      **global-only** enforced structurally (repo `.thegn.*` carries no live
      `[metrics]`) plus `reject_overlay_command_collectors` +
      `Config::repo_command_collector_warnings` (warn, naming the target) for a
      repo overlay that attempts one. Unit-tested.
- [x] 5.2 `thegn-host/src/metrics.rs`: `collect_command` (argv, no shell,
      timeout via reader thread + kill, output cap) beside the scrape path; same
      health mapping; unix smoke tests (capture/timeout/cap/missing-program).
- [x] 5.3 Documented in `config/config.toml.example` with the global-only rule
      inline.

## 6. Validation

- [ ] 6.1 `just e2e-update` — DEFERRED (e2e is known-broken/stale per repo
      policy; monitor frames changed but baselines aren't re-recordable now).
- [ ] 6.2 `just ci` — run once at PR prep (not per-edit). Scoped
      `just quick`/nextest used during implementation.
