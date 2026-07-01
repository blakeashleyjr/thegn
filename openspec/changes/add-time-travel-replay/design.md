# Design

## Recorder (Phase 1)

`PtyPane` gains `record: Option<Recording>`. `feed()` appends each `Output` chunk
as `Event { at_ms, bytes }` after advancing the emulator; every
`KEYFRAME_INTERVAL` (4s activity OR 256KiB) it records a keyframe = `{ at_ms,
event_idx, rows, cols }` (a byte-log marker, not a grid dump). Seeking to T: binary
search the marker ≤ T, spin a fresh `AlacrittyEmulator::new(rows,cols,scroll)`,
re-feed the bounded byte slice — exact by construction, **zero emulator-trait
changes**. Bounded by bytes + time (`[replay] max_bytes_per_pane=8388608`,
`max_duration_secs=1800` — plain numeric, matching the codebase's `idle_ttl_secs`
convention; no humansize deserializer exists); eviction drops front events and
orphaned keyframes. `enabled=false` ⇒ `None`, one null check, zero allocation
(default is **enabled=true**). `Instant::now()` on the loop is a vDSO read — no
wakeups added. Optional `persist=true` mirrors the ring to
`replay/<session>/<pane>.szr` on a dedicated off-loop writer thread
(diff-watcher pattern).

## Replay UI (Phase 2)

`ReplayOverlay` (sibling of `SearchOverlay`, captures all keys) paints from a
**scratch** emulator, never the live pane. Playback clock = a ticker thread that
exists **only while playing** (pulses `TerminalWaker`, parks on pause/exit) — an
event producer, not a poll, so idle stays 0 wakeups. Skip-inactivity collapses
gaps > `idle_threshold_ms`. Time-search re-feeds frames and tests grid text
extracted by **iterating `cell(row,col).text` over every row** (styling-agnostic)
so it finds strings that only ever appeared inside alt-screen apps — crucially
**not** `row_text`, which returns `None` for any styled row (exactly the
alt-screen case). Runs on `spawn_blocking`, streamed back over a channel. Time
expressions (`1h30s`) via a `parse_duration` helper.

## Registers (Phase 3) + screen swap (Phase 4)

Registers (`registers.rs`, pure/coverage-gated) persisted in a new `registers`
table (DB **v27**, additive `CREATE TABLE IF NOT EXISTS`; current schema is 26);
`"+` = system clipboard (not persisted; needs a new `clipboard::paste()`). Alt/main
screen swap ships via the replay-subsumed path; the live toggle is deferred pending
a check of alacritty's public API for reading the inactive (retained main) screen —
if not cleanly exposed, replay already answers the need, so don't fork the emulator.

## Invariants

Recorder is the only always-on addition and now ships enabled by default, so its
cost is measured as a before/after perf delta (`just bench`: launch→first-frame +
idle) per the perf-commit convention, and the `render_plan` invariants (idle⇒Skip,
pane-output⇒Panes) must stay green. The playback clock is the only timer and is
scoped to active playback; everything else rides the waker. Tests isolate
`XDG_STATE_HOME` and the replay dir.
