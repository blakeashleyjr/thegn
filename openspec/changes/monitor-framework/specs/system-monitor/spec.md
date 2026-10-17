## ADDED Requirements

### Requirement: The monitor board fills the overlay

The system monitor SHALL arrange its panels as a deterministic tiled board that
fills the overlay interior with one shared border and no overlapping panels.
At 240×60, the board MUST reach a fill ratio of at least 0.55. When space is
limited, it SHALL downgrade panel presentations and then drop panels by
priority, monotonically and deterministically, showing the dropped count as
`+N hidden`. Below 56×16, it SHALL use a stacked scrolling fallback.

#### Scenario: The board uses the available area

- **WHEN** the monitor opens at 240×60
- **THEN** panels tile the interior with one shared border and no overlap
- **AND** the panel fill ratio is at least 0.55

#### Scenario: The board degrades in a narrow area

- **WHEN** the available interior becomes too small for all current panel sizes
- **THEN** lower-priority panels downgrade before any panel is dropped
- **AND** dropped panels are counted as `+N hidden` in a deterministic,
  monotonic layout

#### Scenario: The terminal is below the board minimum

- **WHEN** the monitor interior is narrower than 56 columns or shorter than 16
  rows
- **THEN** panels use a stacked scrolling fallback

### Requirement: A panel with no source is removed before layout

A panel whose source health is Absent MUST be removed before layout so remaining
panels can use its space. A Warming source SHALL render `…`; a Denied source
SHALL render one ghosted reason line. The monitor MUST NOT render zero as a
substitute for a missing metric.

#### Scenario: A source is absent

- **WHEN** a panel's source reports Absent
- **THEN** the panel is removed before the board is laid out
- **AND** neighbouring panels can use the released space

#### Scenario: A source is warming or denied

- **WHEN** a panel source reports Warming
- **THEN** its panel shows `…`
- **AND** when a source reports Denied, its panel shows one ghosted reason line

#### Scenario: A metric is missing

- **WHEN** a metric has no sampled value
- **THEN** the panel shows its unavailable state or gap
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

The monitor SHALL keep T0 history at 600 one-second samples, T1 at 720 five-
second mean/maximum buckets, and T2 at 1440 sixty-second mean/maximum buckets.
Class A, B, and C series MUST each have explicit caps under a compile-time
8 MiB total budget. Pushes MUST allocate nothing. Skipped buckets SHALL be NaN
gaps rather than zeroes, and history MUST NOT persist across restarts.

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

In a release build at 240×60, monitor board rebuild MUST take no more than
2 ms, blit MUST take no more than 0.3 ms, and rendering MUST allocate zero
after warm-up. Rasters SHALL be computed only during repaint. The perf workload
measures these budgets; wall-clock CI does not.

#### Scenario: The perf workload measures a release frame

- **WHEN** the monitor perf workload runs in a release build at 240×60
- **THEN** board rebuild is measured against a 2 ms budget
- **AND** blit is measured against a 0.3 ms budget with zero post-warm-up
  allocations

#### Scenario: No repaint is needed

- **WHEN** an idle wake has no new monitor frame or chrome damage
- **THEN** graph rasters are not recomputed
- **AND** render planning remains `Skip`

### Requirement: Overview is the first monitor tab

Overview SHALL be tab 1 and the default landing tab only when no valid saved
tab exists. The monitor MUST persist `last_tab` by slug, and a valid saved slug
SHALL take precedence over the default so existing users retain their tab.

#### Scenario: A new user opens the monitor

- **WHEN** no valid `last_tab` slug is saved
- **THEN** the monitor opens on Overview at tab 1

#### Scenario: A returning user opens the monitor

- **WHEN** a valid `last_tab` slug is saved
- **THEN** the monitor opens on that tab
- **AND** the saved slug wins over the Overview default

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
