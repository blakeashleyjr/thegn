## MODIFIED Requirements

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

## ADDED Requirements

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
