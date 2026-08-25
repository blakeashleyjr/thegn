# Design — extend-system-monitoring

## Context: what the audit found

| Layer            | Where                                                  | State                                                                                                                                                                                                                                                                                        |
| ---------------- | ------------------------------------------------------ | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Sampler          | `crates/thegn-metrics` (`StatsSampler`, `ProcSampler`) | Solid. Per-OS leaf crate over `sysinfo` + native paths (Linux sysfs GPU/battery/vmstat, HID temps on Apple silicon). `Option`/empty = absent. Platform-cfg ratchet (`test/platform-cfg-metrics-ratchet.txt`). Cross-platform contract test (`sample_is_well_formed`) runs on all three OSes. |
| Ambient UI       | masthead widgets (`[bars]`/`[stats]`), bar-chip popups | Solid; themed threshold colors; absent metric hides its widget.                                                                                                                                                                                                                              |
| Alerts           | `[stats.alerts]` → `thegn_core::resource_alert`        | Solid; sustain + repeat-cap + hysteresis; includes the direct-reclaim rule and per-core load.                                                                                                                                                                                                |
| Deep view        | monitor modal (`monitor.rs`)                           | Solid; 8 hardware-gated tabs; history rings + window ladder; pause; visibility-gated process enumeration with pane/daemon attribution.                                                                                                                                                       |
| External metrics | `[metrics]` scraper → sidebar METRICS                  | Prometheus text format over HTTP only.                                                                                                                                                                                                                                                       |
| Escape hatches   | `[monitor] system`/`gpu`, `[[tools]]`, btop pin        | Fine as-is; this is where bottom/btop/nvtop belong.                                                                                                                                                                                                                                          |
| Spec             | `openspec/specs/`                                      | **Nothing.** The whole stack is unspecced.                                                                                                                                                                                                                                                   |

Reference-shelf triage (from THE-44's links):

- **bottom** — the full-monitor ceiling; thegn's modal deliberately stops
  short (no zoomable multi-chart dashboard). `[monitor] system = "btm"`
  already launches it. Non-goal.
- **procs** — search/tree/extra columns over ps. Filter + tree + signal are
  in-lane (find and stop the runaway build eating the box); ports-per-process
  and full column config are not.
- **dust / duf** — per-mount free is already the Disk tab (`disks:
Vec<DiskInfo>` covers duf); dust's tree walk is off-lane, but its question
  ("where did the disk go?") has an IDE-shaped answer: worktrees and their
  `target/` dirs, which the `[disk]` scanner already measures into the DB.
- **diskwatch** — SMART/latency histograms off-lane; its "days-to-full"
  projection is a cheap, honest derivative of history thegn already keeps.
- **netwatch** — pcap, root, L7 decode: firmly off-lane. Per-container net is
  THE-45's lane (engine stats already carry it).
- **syswatch** — closest cousin (sysinfo, tabs, honest platform gaps); its
  "insights/anomaly" layer maps to `[stats.alerts]`, which already exists.
  Its session record/replay is interesting but unjudgeable now; deferred.

## Decisions

### D1. No provider seam for local metrics — pin the contract instead

"Seams, not vendors" targets backends with substitutable vendors. Local
sampling has none: `sysinfo` is a substrate (like termwiz), and the per-OS
variation is handled _inside_ the leaf crate under its own ratchet. Forcing
`StatsSampler` behind `thegn_core::seam` would add an object-safe indirection
with exactly one implementation and no probe question to answer.

The place where metrics _are_ seam-shaped already has one in flight: the
Observe `DataSource` trait (`add-observability-dashboards`), whose `host`
source wraps `thegn-metrics`. This change stays out of its way.

What was genuinely missing is the **written contract** the port work keeps
tripping over: which fields may be absent where, that absence hides the
surface rather than rendering zero, and that no sampling ever rides the event
loop. That becomes the `system-monitor` spec.

### D2. Doctor coverage report

`thegn doctor` gains a "system metrics" block: one line per metric family
(cpu, cores, freq, temp, mem, swap, reclaim, gpu, net, battery, disk, load,
uptime) with `available` or `absent (<reason>)`. Reasons distinguish
_not-on-this-OS_ (reclaim on macOS), _no-hardware_ (no battery, no NVIDIA
adapter), and _no-permission_ where detectable. Implementation is a pure
classification over one `StatsSnapshot` pair (two samples for the deltas) —
unit-testable in core-free `thegn-metrics`, printed by the host.

### D3. Processes tab: filter, tree, signal

- `/` opens an incremental filter over name/pid/owner (owner matches the
  `Pane(n)`/`self`/`daemon` attribution already computed). Pure list filter in
  the overlay; the sampler is untouched.
- `t` toggles tree grouping (ppid chain, already sampled). The top-N union
  the sampler keeps means some parents may be outside the kept set; the tree
  view parents to the nearest kept ancestor and says so, rather than
  enumerating everything.
- `x` (then a confirm prompt) sends SIGTERM to the selected process; a second
  `x` on the same already-TERMed process offers SIGKILL. Failures (ESRCH,
  EPERM) surface in the modal footer — never swallowed. Windows: TERM maps to
  a console-friendly terminate; the confirm text says what will happen.
- The signal path is a host-side syscall on the selected pid — no subprocess,
  no event-loop block. It is **TUI-only**: not projected to CLI/control/MCP,
  so no capability-catalog row. If a remote kill is ever wanted it must arrive
  as a catalog row with a `write` scope; this change deliberately does not
  open that door (the daemon's `sessions.kill` already covers thegn's own
  sessions remotely).

### D4. Disk tab: the worktree lane

Two additions, both served from data thegn already collects:

- **Per-worktree usage list**: rows from the `[disk]` scanner's DB cache
  (worktree, total, `target/` share, age of measurement), sorted by size,
  with the existing clean action (`auto_clean_on_merge`'s manual sibling)
  invocable per row. No filesystem walk is triggered by opening the tab; a
  stale row shows its age instead of blocking on a fresh `du`.
- **Days-to-full projection**: linear fit over the worktrees-filesystem free
  bytes already in the monitor's history ring; shown only when the trend is
  downward and the ring holds enough span to be honest (else "n/a"). An
  optional `[stats.alerts] disk_eta = { warn_hours, critical_hours }` rule
  fires through the existing sustain/hysteresis machinery. Default off (0).

### D5. Command collectors in `[[metrics.targets]]`

```toml
[[metrics.targets]]
name = "gpu-vendor-x"
kind = "command"                  # default: "prometheus"
command = ["vendor-smi", "--prometheus"]
timeout_ms = 500                  # same key as scrape targets
metrics = ["vendorx_gpu_busy"]    # same allowlist filter
```

- Runs on the existing metrics supervisor thread at the target's interval;
  argv exec (no shell), bounded output (`max_body_bytes` applies), stdout
  parsed by the existing Prometheus text parser; failures/timeouts mark the
  target `Error`/`Stale` exactly like a failed scrape. Nothing new touches
  the event loop.
- **Global config only.** A repo overlay adding an argv to execute is a
  config-driven code-execution door; until `add-config-trust-resolution`
  gives it a gate, repo/workspace overlays MUST NOT be able to define or
  modify command collectors. The config loader rejects them from overlay
  layers with a visible warning.
- This is also the plugin story's substrate: a future plugin-registered
  collector is just a row in the same table (out of scope here, noted for
  `plugin-api`).

### Alternatives considered

- **A `MetricsProvider` seam in `thegn_core::seam`** — rejected (D1): one
  implementation, no vendor axis, and it would push per-OS knowledge out of
  the leaf crate that ratchets it.
- **A new "System" panel section for processes/disk** — rejected: the monitor
  modal is the established deep-dive surface and already owns the keys,
  history and gating; the panel's Sandbox/System sections stay summary-level.
- **StatsD/OTLP ingest instead of command collectors** — rejected for now:
  a listening socket is a bigger attack/config surface than exec-and-parse,
  and Prometheus text format is the format the stack already speaks end to
  end. Revisit if Observe grows an OTLP source.
- **Per-process kill via `sessions.kill`** — rejected: that verb kills a
  daemon session (a pane), not an arbitrary pid; conflating them would give
  remote clients pid-level kill by accident.

## Event-loop / render notes

- No new wake sources. The monitor continues to ride the stats drain; filter
  and tree are pure view-state; the signal action is a syscall handled in the
  key handler (non-blocking).
- Render damage: all monitor changes are overlay content → `Full` frames only
  while the modal is open, unchanged from today. Doctor is CLI-only.
- Command collectors run on the metrics supervisor thread (already off-loop,
  channel + waker delivery); QoS `Background` like its scrape siblings.

## Security

- **Command collectors execute configured argv.** Mitigations: global config
  only (overlay layers rejected — see D5), argv not shell, timeout + output
  cap, and the values only ever flow into the existing allowlist-filtered
  metrics model. No credentials are involved; a collector needing a token
  reads it from its own environment, never from thegn config.
- **Process signal is host-level power** bounded to what the thegn user could
  already do in any pane (`kill` respects OS permissions; EPERM is surfaced).
  It is not exposed to any external surface — no catalog row, no scope to
  mis-grant. The confirm step names the pid, process name, and owner
  attribution so a pane-owned build is recognizably thegn's own.
- **Doctor coverage report** reads only local sampler state; no new I/O.
- No new write surface to the DB; the disk lane reads the existing cache.

## Open questions

- Should the days-to-full projection use a robust (median-of-slopes) fit
  rather than least-squares? Cheap either way; decide at implementation with
  a fixture of a bursty build trace.
- `syswatch`-style session record/replay of monitor history (dump/load the
  rings) — useful for "what happened while I was away", but the state-db
  cost and retention story need their own judgement. Deferred; noted for AH.
- Whether the coverage report should also be `thegn doctor --json` structured
  output for the e2e/platform CI matrix — assumed yes at implementation
  (doctor already emits JSON blocks).
