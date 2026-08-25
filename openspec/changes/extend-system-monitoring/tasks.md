# Tasks — extend-system-monitoring

## 1. Pin the existing contract (verify + test, no new behaviour)

- [ ] 1.1 Sweep the sampling paths against the spec: `StatsSampler` on the
      refresh ticker, `ProcSampler` on its own thread, both `Qos::Background`,
      channel + waker delivery; add/point-to unit tests where a clause isn't
      already covered (`sample_is_well_formed`, monitor gating tests).
- [ ] 1.2 Verify the absent-metric rule end to end (widget hide, tab hide,
      inert alert on absent metric) and add a regression test for the
      alert-never-fires-on-absent clause in `thegn_core::resource_alert`.

## 2. Doctor coverage report

- [ ] 2.1 `thegn-metrics`: pure `coverage(snapshot_pair) -> Vec<FamilyReport>`
      classification (available / absent{os, hardware, permission}) + unit
      tests per platform-representative fixture.
- [ ] 2.2 `cmd/doctor.rs`: print the block (and include it in the JSON
      output); keep the two-sample warmup off the fast path.

## 3. Processes tab

- [ ] 3.1 Overlay filter state (`/`, incremental, Esc clears) over the kept
      rows; pure view logic + tests in `monitor/`.
- [ ] 3.2 Tree grouping toggle from sampled ppid chains, with
      nearest-kept-ancestor elision; pure + tested.
- [ ] 3.3 Signal action: confirm prompt (pid, name, owner), TERM then
      KILL-on-second-confirm, error surfacing; Windows terminate mapping in
      `platform/`. New action ids registered in `ACTION_SPECS`.
- [ ] 3.4 Update `docs/help/system-monitor.md` (claims the new action ids —
      help ratchet).

## 4. Disk tab worktree lane

- [ ] 4.1 Feed the `[disk]` scanner cache rows (size, target share, age) into
      the monitor model; render sorted list; wire the existing clean action.
- [ ] 4.2 Days-to-full projection over the free-bytes ring: pure fit +
      honesty threshold in core (unit-tested), rendered in the tab header.
- [ ] 4.3 `[stats.alerts] disk_eta = { warn_hours, critical_hours }` (default
      0/off) through `resource_alert`; config example entry; tests.

## 5. Command collectors

- [ ] 5.1 `thegn_core::config` / `metrics`: `kind` field on
      `[[metrics.targets]]` (`prometheus` default, `command`), `command` argv;
      overlay-layer rejection with warning; validation + unit tests.
- [ ] 5.2 `thegn-host/src/metrics.rs`: exec path (argv, no shell, timeout,
      output cap) beside the scrape path; same health mapping; smoke-level
      test with a stub script.
- [ ] 5.3 Document in `config/config.toml.example`; note the global-only rule
      inline.

## 6. Validation

- [ ] 6.1 `just e2e-update` for monitor frames that changed (filter/tree/disk
      lane), reviewing diffs; pin any new volatile chrome in `e2e_freeze`.
- [ ] 6.2 Run `just ci` once at the end (includes openspec validate, lint,
      coverage on core logic).
