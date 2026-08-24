# System Monitor

## ADDED Requirements

### Requirement: All system sampling runs off the event loop

System metrics SHALL be sampled on background threads only — the periodic
`StatsSampler` on the refresh-ticker thread and the per-process `ProcSampler`
on its own thread — each declaring background thread QoS, delivering snapshots
over a channel and pulsing the terminal waker. An open monitor surface MUST
NOT add a timer or wake source of its own, and no sampling work may run on the
event loop or before the first frame.

#### Scenario: Idle stays idle with the monitor open

- **WHEN** the monitor modal is open and no input or sample arrives
- **THEN** the event loop remains blocked with no additional wake source and
  an idle wake still plans a `Skip` frame

#### Scenario: Samples arrive via channel and waker

- **WHEN** the ticker thread completes a sample
- **THEN** the snapshot is sent on a channel and the terminal waker is pulsed,
  and the loop drains it as ordinary work

### Requirement: A metric a platform cannot supply hides its surface

Every snapshot field SHALL be optional (or an empty collection), and a metric
the current platform, hardware, or permissions cannot supply MUST render as an
absent widget, hidden tab, or omitted row — never as zero, a placeholder
value, or an error. Threshold alerts on an absent metric MUST never fire and
MUST never report recovery.

#### Scenario: No load average on Windows

- **WHEN** thegn runs on a platform without a load average
- **THEN** the load widget does not render and the load alert rule stays inert

#### Scenario: A metric disappears while displayed

- **WHEN** the hardware behind a monitor tab goes away (battery unplugged,
  disk unmounted)
- **THEN** the tab hides and focus moves to a tab that still has content

### Requirement: Doctor reports per-platform metric coverage

`thegn doctor` SHALL report, for each metric family the sampler models (cpu,
per-core, frequency, temperature, memory, swap, direct-reclaim, gpu, network,
battery, disk, load, uptime), whether this platform and machine yields it,
and for an absent family the reason class: not implemented on this OS, no
such hardware, or no permission. The classification SHALL be pure logic over
sampled snapshots, unit-tested in the metrics crate.

#### Scenario: Linux box without an NVIDIA adapter

- **WHEN** `thegn doctor` runs on Linux with no GPU exposed by sysfs or
  `nvidia-smi`
- **THEN** the gpu family is reported absent with a no-hardware reason while
  cpu/memory/disk report available

#### Scenario: Coverage matches the UI

- **WHEN** doctor reports a family absent
- **THEN** the corresponding masthead widget and monitor tab are also absent
  for the same reason

### Requirement: The process table is filterable, groupable, and can signal a process

The monitor's Processes tab SHALL support an incremental filter over process
name, pid, and owner attribution (self / daemon / pane / other), and a tree
grouping toggle built from the sampled parent chain; when a parent falls
outside the kept top-N set the row SHALL parent to its nearest kept ancestor
and indicate the elision. The tab SHALL offer a signal action on the selected
process that requires explicit confirmation, sends a termination signal first
and offers a kill signal only as a distinct second confirmation, and surfaces
signal failures (no such process, permission denied) rather than swallowing
them. The signal action MUST be a TUI-only surface — it SHALL NOT be projected
to the CLI, control API, MCP, or plugin surfaces, and therefore adds no
capability-catalog row.

#### Scenario: Filtering the process list

- **WHEN** the user presses `/` on the Processes tab and types a fragment
- **THEN** only rows whose name, pid, or owner match remain, without a fresh
  enumeration pass

#### Scenario: Killing a runaway build

- **WHEN** the user invokes the signal action on a pane-owned process and
  confirms
- **THEN** SIGTERM is sent to that pid, the outcome is shown, and a kill
  signal is only sent after a further explicit confirmation

#### Scenario: Signal failure is surfaced

- **WHEN** a signal returns permission-denied
- **THEN** the failure is displayed in the monitor and nothing is retried
  silently

### Requirement: The Disk tab shows worktree usage and a fill projection

The monitor's Disk tab SHALL list per-worktree usage from the existing disk
scanner cache — total size, build-artifact (`target/`) share, and measurement
age — sorted by size, offering the existing clean action per row; opening the
tab MUST NOT trigger a filesystem walk, and a stale measurement SHALL show its
age rather than block on a rescan. The tab SHALL show a time-to-full
projection for the worktrees filesystem derived from recorded free-space
history, shown only when the trend is downward and the history span supports
it. An optional `disk_eta` alert rule (off by default) SHALL fire through the
standard alert sustain/hysteresis machinery when the projection falls below
its configured hours.

#### Scenario: Worktree usage from cache

- **WHEN** the user opens the Disk tab
- **THEN** worktrees are listed with cached sizes and `target/` share, and no
  `du` walk starts as a consequence of opening it

#### Scenario: Projection is honest about thin history

- **WHEN** free-space history covers too little span to extrapolate
- **THEN** the projection shows as unavailable rather than a confident number

#### Scenario: Cleaning a fat worktree

- **WHEN** the user invokes clean on a listed worktree
- **THEN** the existing clean path runs off the event loop and the row updates
  after the next measurement

### Requirement: Metrics targets accept command collectors

The `[[metrics.targets]]` table SHALL accept `kind = "prometheus"` (the
default, today's HTTP scrape) or `kind = "command"`. A command collector runs
a configured argv — never through a shell — on the metrics supervisor thread
at the target interval, with the target timeout and response-size cap
applied, and parses its stdout as Prometheus text format; its samples flow
through the same allowlist filter, health states (up/stale/error), and UI
surfaces as scraped targets. A failing or timing-out collector marks its
target unhealthy exactly like a failed scrape and MUST NOT block the event
loop or delay other targets. Command collectors MUST be definable in the
global config only: a repo or workspace overlay attempting to define or
modify one SHALL be rejected with a visible warning.

#### Scenario: A command collector feeds the sidebar

- **WHEN** a `kind = "command"` target's argv prints a gauge in Prometheus
  text format and the metric is allowlisted
- **THEN** the value appears in the sidebar METRICS section like any scraped
  sample

#### Scenario: A hung collector degrades, not blocks

- **WHEN** a collector exceeds its timeout
- **THEN** it is killed, the target shows an error state, and other targets
  refresh on schedule

#### Scenario: Repo overlay cannot inject a collector

- **WHEN** a repo-level config layer defines a `kind = "command"` target
- **THEN** the target is rejected at load with a warning naming the layer, and
  no command runs
