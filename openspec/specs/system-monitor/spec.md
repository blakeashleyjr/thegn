# system-monitor Specification

## Purpose

TBD - created by archiving change extend-system-monitoring. Update Purpose after archive.

## Requirements

### Requirement: All system sampling runs off the event loop

System metrics SHALL be sampled on background threads only — the periodic
`StatsSampler` on the refresh-ticker thread, the per-process `ProcSampler` on
its own thread, and the deep `MonitorSampler` on the `thegn-monitor` thread
with Utility QoS. Each worker SHALL deliver its snapshots or frames over a
channel and pulse the terminal waker. The deep monitor sampler MUST exist only
while the monitor is open and MUST park without a timed wait while the monitor
is paused or covered. An open monitor surface MUST NOT add a timer or wake
source of its own, and no sampling work may run on the event loop or before
the first frame.

#### Scenario: Idle stays idle with the monitor open

- **WHEN** the monitor modal is open and no input or sample arrives
- **THEN** the event loop remains blocked with no additional wake source and
  an idle wake still plans a `Skip` frame

#### Scenario: Samples arrive via channel and waker

- **WHEN** the ticker thread completes a sample
- **THEN** the snapshot is sent on a channel and the terminal waker is pulsed,
  and the loop drains it as ordinary work

#### Scenario: Deep monitor frames arrive via the sampler

- **WHEN** the open, visible monitor's deep sampler publishes a frame
- **THEN** the frame is delivered through the three-frame latest-wins pool and
  the terminal waker is pulsed at most once for that published frame
- **AND** while the monitor is paused or covered, the sampler parks without a
  timed wait
- **AND** closing the monitor stops and joins the sampler

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

### Requirement: Hidden process sampling is parked and owned

The process sampler SHALL use fallible tracked thread admission, indefinite
hidden/paused parking, and at least two seconds between enabled scan starts.
It SHALL retain at most one unpublished snapshot and preserve a separate
in-flight collection. Cancellation SHALL revoke stale publication, and a parked
acknowledgement SHALL follow any earlier collection, wake attempt and reset.
An unfinished OS call SHALL remain owned with a Held cleanup outcome under the
application deadline. Failure SHALL be observable without requiring a sample.

#### Scenario: Processes is hidden or paused

- **WHEN** the worker acknowledges the current hidden request
- **THEN** it schedules no timed wait, collection or publication until a new enabled request
- **AND** consumer closure wakes cancellation without requiring another scan

#### Scenario: Hide and reopen race with collection or terminal notification

- **WHEN** a prior-generation collection or wake remains in flight across hide and reopen
- **THEN** the old result is not delivered and the reset is not skipped
- **AND** the first fresh sample is unprimed while scan starts respect the minimum interval

#### Scenario: Cleanup deadline precedes actual thread return

- **WHEN** cancellation occurs during an OS call or after a completion receipt but before thread return
- **THEN** cleanup reports Held and retains the exact handle
- **AND** new worker admission cannot replace it until exact finished-handle retirement

### Requirement: Monitor invalidation follows relevant content

The monitor SHALL avoid rebuilding inactive process/disk rows and avoid
refreshing unchanged Processes content for unrelated stats publications.
Publication revisions SHALL cover all sampled values, ownership and birth
identity and survive hydration. Graph time progression and displayed disk ages
SHALL remain independent of row ordering caches.

#### Scenario: An unrelated stats snapshot arrives

- **WHEN** process data, view inputs and viewport remain unchanged
- **THEN** Processes does not rebuild rows or request repaint
- **AND** a visible graph still advances according to its existing time and history inputs

### Requirement: Process columns have stable cell boundaries

Only the Processes table SHALL use fixed viewport-derived column boundaries.
It SHALL measure and clip the same sanitized display cells, preserving sampled
selection and signal-confirmation identity for the rendered row.

#### Scenario: Sampled ranks and Unicode names change at a fixed viewport

- **WHEN** CPU, memory, ownership labels, process ranks or Unicode names change
- **THEN** process column starts remain fixed and narrow cells clip safely
- **AND** the selected rendered row and signal target refer to the same sampled identity

### Requirement: Process snapshots survive unrelated hydration

The latest process snapshot SHALL remain available across Git/DB model hydration
until the process sampler supplies a replacement. Hydration MUST NOT create an
empty process-table interval or reset its cursor between process samples.

#### Scenario: Hydration lands between process samples

- **WHEN** a populated process snapshot is displayed and unrelated model hydration arrives
- **THEN** its rows, sampled values, selection, and viewport remain available without a sampling placeholder
- **AND** a later process sample replaces the snapshot normally

### Requirement: Process refresh ordering and identity remain coherent

The bounded CPU/RSS retained set and flat/tree row ordering SHALL resolve equal
sort keys deterministically by PID. Passive refresh SHALL retain the selected
sampled process identity when it remains in the retained list, and otherwise
clamp the old cursor position to the remaining rows. Explicit sort and filter
commands SHALL retain their existing selection semantics. Manual wheel scrolling
MUST NOT be pulled back to the selection by a passive sample.

For every sort key (CPU, memory, Name, and PID), the displayed down arrow SHALL
mean descending values and the displayed up arrow SHALL mean ascending values.
Name ordering SHALL use the existing lexical comparator. The direction contract
MUST apply in both flat and tree views, including roots, siblings, and cycle
leftovers, while retaining the PID tie-break for equal CPU, memory, or Name
values.

#### Scenario: Many idle processes tie at the retained cutoff

- **WHEN** process enumeration returns more tied CPU/RSS rows than the retained limit in a different order
- **THEN** the retained set and displayed tie order remain identical

#### Scenario: The selected process changes rank

- **WHEN** a passive process sample changes the selected process's sorted position
- **THEN** the selection follows its sampled PID and birth time
- **AND** the visible selected row and signal prompt refer to that same sampled process

#### Scenario: A pending signal target changes sampled identity

- **WHEN** the process identity named by a pending confirmation has vanished or has a different sampled birth time in the latest displayed snapshot
- **THEN** confirming refuses the action and asks for a fresh selection
- **AND** a reused PID does not inherit an earlier process's signal escalation state

#### Scenario: Sort arrows match all process keys

- **WHEN** the user selects CPU, memory, Name, or PID and toggles the existing
  reverse key
- **THEN** the rendered key label and arrow agree with the exact ascending or
  descending order in flat and tree views
- **AND** equal CPU, memory, or Name values remain ordered by ascending PID

### Requirement: Overview landing tab

The system monitor SHALL place Overview first, assign it key 1, and use it as
the default tab only when no valid saved `last_tab` slug exists. The monitor
MUST persist `last_tab` by slug, and a valid saved slug SHALL take precedence
over the Overview default so existing users retain their tab.

#### Scenario: A new user opens the monitor

- **WHEN** no valid `last_tab` slug is saved
- **THEN** the monitor opens on Overview at tab 1

#### Scenario: A returning user opens the monitor

- **WHEN** a valid `last_tab` slug is saved
- **THEN** the monitor opens on that tab
- **AND** the saved slug wins over the Overview default

### Requirement: Overview source-aware panels

Overview SHALL show summary panels for CPU, load and pressure, memory,
network, disk, thermal, power when present, and process/container counts.
Panels without an available source SHALL be omitted and SHALL NOT add their
collectors to sampler demand. Missing readings SHALL use the terminal absent
mark rather than a numeric zero.

#### Scenario: Power source is absent

- **WHEN** no power supply reading is present
- **THEN** the power panel is not placed
- **AND** the power-supply collector is not demanded by Overview

### Requirement: Frozen Overview frame

The monitor SHALL copy a sampled frame into overlay-owned Overview state before
the sampler recycles the frame. A paused overlay SHALL ignore incoming frames.

#### Scenario: Paused Overview receives a frame

- **WHEN** the overlay is paused and a sampled frame arrives
- **THEN** the Overview model is unchanged
- **AND** the frame is not marked dirty

### Requirement: The monitor board fills the overlay

The Overview tab SHALL arrange its panels as a deterministic tiled board that
fills the overlay interior with one shared border and no overlapping panels.
At 240×60, its panel fill ratio MUST be at least 0.55. When space is limited,
it SHALL downgrade panel presentations and then drop panels by priority,
monotonically and deterministically, showing the dropped count as `+N hidden`.
The monitor overlay interior SHALL be clamped to a minimum of 56×16 where the
screen permits; tab content scrolls within the available interior.

#### Scenario: The Overview board uses the available area

- **WHEN** the monitor opens at 240×60
- **THEN** Overview panels tile the interior with one shared border and no
  overlap
- **AND** the panel fill ratio is at least 0.55

#### Scenario: The board degrades in a narrow area

- **WHEN** the available interior becomes too small for all current panel sizes
- **THEN** lower-priority panels downgrade before any panel is dropped
- **AND** dropped panels are counted as `+N hidden` in a deterministic,
  monotonic layout

### Requirement: A panel with no source is removed before layout

Overview panels whose source health is Absent MUST be removed before layout so
remaining panels can use their space. The Overview power panel SHALL be placed
only once the power-supply source reports Ok or Failing, or base stats carry a
battery reading. Other board views may retain a panel for an unavailable
collector and render its absent mark; missing readings MUST NOT be displayed as
zero. CPU and GPU board views render the active ellipsis glyph for a Warming
source; disk maps Warming and Denied readings to its state marks.

#### Scenario: An Overview source is absent

- **WHEN** an Overview panel's source reports Absent
- **THEN** that panel is removed before the board is laid out
- **AND** neighbouring panels can use the released space

#### Scenario: Power discovery is incomplete

- **WHEN** the power-supply source has not reported Ok or Failing and base stats
  carry no battery reading
- **THEN** the Overview power panel is not placed
- **AND** the power-supply collector is not demanded by that panel

#### Scenario: A board panel has a missing metric

- **WHEN** a board panel has no sampled value for a metric
- **THEN** it renders its unavailable mark or gap
- **AND** it does not display zero for that missing value

### Requirement: The deep monitor sampler exists only while the monitor is open

The deep `MonitorSampler` MUST own no thread, file descriptor, or timer while
the monitor is closed. It SHALL start on open, stop and join with its file
descriptors closed on close, and park without a timed wait while hidden or
paused. It MUST publish latest-wins through a pool of three frames and pulse
the `TerminalWaker` at most once per published frame. `StatsSnapshot` MUST NOT
be widened for deep monitor data.

#### Scenario: Opening and closing the monitor owns the sampler

- **WHEN** the monitor opens
- **THEN** the deep sampler starts without adding an event-loop timer
- **AND** when the monitor closes, the sampler stops and joins with its file
  descriptors closed

#### Scenario: The monitor is hidden or paused

- **WHEN** the monitor is hidden or paused
- **THEN** the deep sampler parks without a timed wait
- **AND** the base stats sampler continues to serve masthead data
- **AND** masthead Class-A history continues while per-instance series show
  honest gaps

#### Scenario: Frames are published under load

- **WHEN** the sampler produces frames faster than the UI consumes them
- **THEN** it publishes latest-wins through a three-frame pool
- **AND** it pulses the terminal waker at most once for each published frame

#### Scenario: Base stats remain narrow

- **WHEN** deep monitor readings are collected
- **THEN** they remain outside `StatsSnapshot`

### Requirement: Monitor history is tiered and bounded

The monitor SHALL keep T0 history at 600 one-second samples, T1 at 720
five-second mean/maximum buckets, and T2 at 1440 sixty-second
mean/maximum buckets. Class A, B, and C series MUST each have explicit caps
under a compile-time 8 MiB total budget. Pushes MUST allocate nothing. Skipped
buckets SHALL be NaN gaps rather than zeroes, and history MUST NOT persist
across restarts.

#### Scenario: Samples roll into bounded tiers

- **WHEN** samples accumulate across the configured history intervals
- **THEN** T0 retains 600 one-second samples, T1 retains 720 five-second
  mean/maximum buckets, and T2 retains 1440 sixty-second mean/maximum buckets
- **AND** class caps keep total series storage within the compile-time 8 MiB
  budget

#### Scenario: A sample bucket is skipped

- **WHEN** no sample exists for a history bucket
- **THEN** the bucket is represented as NaN
- **AND** rendering preserves the gap instead of drawing a zero

#### Scenario: History is updated and restarted

- **WHEN** a sample is pushed into a series
- **THEN** the push allocates no memory
- **AND** a process restart begins with no persisted monitor history

### Requirement: The monitor stays within its frame budget

At 240×60, the monitor perf workload SHALL report Overview and CPU model
rebuild and paint medians against reference targets of 2 ms and 0.3 ms. Those
wall-clock targets are informational and are not enforced by CI. Rendering
allocation tests SHALL keep their pinned allocation ceilings, and rasters SHALL
be computed only during repaint.

#### Scenario: The perf workload reports a frame

- **WHEN** the monitor perf workload runs at 240×60
- **THEN** it reports Overview and CPU model rebuild and paint timing medians
  against the reference targets
- **AND** the CI gates enforce the pinned allocation ceilings, not those
  machine-dependent wall-clock targets

#### Scenario: No repaint is needed

- **WHEN** an idle wake has no new monitor frame or chrome damage
- **THEN** graph rasters are not recomputed
- **AND** render planning remains `Skip`

### Requirement: Monitor keys follow one scheme

Monitor tabs SHALL use one shared key scheme: Enter opens detail or returns to
the row's home tab; `f`/`F` focuses a panel; `z` zooms; `w` changes grouping;
and Escape clears a filter or confirmation, then leaves detail, then zoom, then
closes the monitor. Containers shell uses `e`, and Containers `t` and `r`
require y/n confirmation. The scheme MUST NOT add `ActionSpec` identifiers.

#### Scenario: Shared detail and panel keys work across tabs

- **WHEN** the user presses Enter, `f`/`F`, `z`, or `w` on a monitor tab
- **THEN** the key performs the shared detail, focus, zoom, or grouping action
- **AND** Enter from a detail row can return to that row's home tab

#### Scenario: Escape unwinds monitor state

- **WHEN** a filter or confirmation, detail view, or zoom is active
- **THEN** Escape clears or leaves the most local active state first
- **AND** Escape closes the monitor only after those states are unwound

#### Scenario: Container actions are explicit

- **WHEN** the user opens a shell for a selected container
- **THEN** the shell action uses `e`
- **AND** stop (`t`) and restart (`r`) require a y/n confirmation without adding
  `ActionSpec` identifiers
