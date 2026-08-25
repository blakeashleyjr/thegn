# Extend system monitoring — audit the stack, then add only what the lane needs

Linear: THE-44

## Why

THE-44 asks three questions of the monitoring stack — "Fully abstracted?
Cross platform? Extendable?" — against a reference shelf of standalone
monitors (bottom, dust, duf, procs, diskwatch/netwatch/syswatch).

The audit's headline finding is that thegn already has most of what those
tools do, built in and on-lane, but **none of it is specced**: there is no
`system-monitor` capability in `openspec/specs/` for a stack that spans

- `thegn-metrics` (`StatsSampler`: CPU/cores/freq/temp, mem/swap/direct-reclaim
  rate, GPU, per-interface net, battery+power+ETA, all mounts + IO rates,
  temps, load, uptime, self/daemon process) — a per-OS leaf crate over
  `sysinfo` with its own platform-cfg ratchet;
- the masthead widgets (`[bars]`/`[stats]`) and threshold alerts
  (`[stats.alerts]` with sustain/repeat/hysteresis in
  `thegn_core::resource_alert`);
- the built-in monitor modal (`monitor.rs`: 8 hardware-gated tabs, timestamped
  history rings with a 30s–12h/all window ladder, area/line/spark styles,
  pause, and a visibility-gated `ProcSampler` with pane/daemon ancestry
  attribution);
- the `[metrics]` Prometheus scraper feeding the sidebar METRICS section; and
- the external escape hatches (`[monitor] system`/`gpu`, `[[tools]]`).

Audit answers, which this change turns into contract:

- **Abstracted?** Deliberately not a `thegn_core::seam` provider seam — local
  metrics have no vendor to swap; `sysinfo` is substrate, not vendor. The
  seam-shaped surface is the Observe `DataSource` trait (in-flight
  `add-observability-dashboards`), whose `host` source wraps `thegn-metrics`.
  The right fix is not a seam retrofit but pinning the actual contract: the
  `Option`/empty-field degradation model and the off-loop sampling discipline.
- **Cross platform?** Coverage is real (the contract test `sample_is_well_formed`
  runs on Linux/macOS/Windows; macOS temps come from HID sensors) but ragged at
  the edges (GPU is Linux sysfs + `nvidia-smi` only; reclaim is Linux-only;
  load is unix-only) and **invisible** — nothing reports what this platform can
  and cannot measure. `thegn doctor` should.
- **Extendable?** Only via Prometheus endpoints today. A generic command
  collector closes that hole without a vendor: anything that can print
  Prometheus text format becomes a metrics target.

Judged against the lane (a worktree IDE's ambient monitoring, not a bottom
clone), three expansions earn their place; the rest of the shelf is explicitly
declined below.

## What Changes

1. **Pin the monitoring contract** as a new `system-monitor` capability spec:
   off-loop sampling with Background QoS and no idle wakes, the
   absent-metric-⇒-hidden-surface degradation rule, visibility-gating of the
   one expensive sampler, and alert hysteresis. Mostly codifying verified
   behaviour; the tasks mark verify-and-test vs build.
2. **Make platform coverage probeable**: `thegn doctor` reports per metric
   family what this platform/build yields — available, or absent with the
   reason (no sensor, no adapter, not implemented on this OS) — matching what
   the widgets/tabs would show.
3. **Processes tab, procs-inspired but bounded**: `/` filter (name/pid/owner),
   a process-tree grouping toggle, and a confirmed signal action (TERM, then
   KILL on a second confirm) — the runaway-build story. TUI-only; **no new
   external door**, so no capability-catalog delta.
4. **Disk tab, worktree lane instead of a dust clone**: a per-worktree usage
   list served from the existing `[disk]` scanner cache (total + `target/`
   share) with the existing clean action wired in, plus a days-to-full
   projection derived from the disk history ring and an optional
   `disk_eta` alert rule.
5. **Generic collector extensibility**: `[[metrics.targets]]` grows
   `kind = "prometheus" | "command"`. A command collector runs an argv (no
   shell) off-thread with a timeout and output cap, parses Prometheus text
   format, and flows through the same allowlist/interval/health model as
   scrape targets. Global config only — repo overlays cannot add collectors.

**Declined as off-lane** (documented as non-goals in the design): a full
process-manager/bottom clone, per-connection network attribution (netwatch
needs pcap/root), SMART disk health (diskwatch), and a general directory
du/treemap browser (dust/duf — the drawer's yazi, `[[tools]]`, and the
worktree-scoped view above cover the IDE's actual disk questions).

## Impact

- Roadmap: **AH** (411–418) — 411–417 are done and get their spec; the
  worktree/container-attribution half of **418** stays with
  `add-container-management` (THE-45). Touches **L** (statusbar widgets) only
  by reference.
- Specs: new `system-monitor` capability. No delta to `observe-*` — the
  in-flight `add-observability-dashboards` `host` DataSource reads the same
  `thegn-metrics` snapshot and is unaffected.
- Code (implementation phase): `thegn-metrics` (no new fields required),
  `thegn-host/src/monitor/` (filter/tree/signal, disk lane),
  `thegn-host/src/cmd/doctor.rs` (coverage report),
  `thegn_core::metrics` + `thegn-host/src/metrics.rs` (command collectors),
  `thegn_core::resource_alert` (`disk_eta`), `config/config.toml.example`.
- Help: `docs/help/system-monitor.md` gains the filter/tree/signal keys and
  the disk lane; the config-reference page is generated. New action ids (the
  signal/filter/tree actions) must be claimed there (help ratchet).
- Related in-flight changes: `add-observability-dashboards` (host source;
  collector metrics become visible to Observe through the same scrape state),
  `add-config-trust-resolution` (why command collectors are global-only for
  now), `add-plugin`-family specs (a plugin-registered collector is a natural
  follow-up through the same target table, out of scope here).
- No SQLite schema change. No new capability-catalog rows (no external door).
