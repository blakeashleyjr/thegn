# Design

## Recorder (Phase 1)

`PtyPane` gains `record: Option<Recording>`. `feed()` appends each `Output` chunk
as `Event { at_ms, bytes }` after advancing the emulator; every
`KEYFRAME_INTERVAL` (4s activity OR 256KiB) it records a keyframe = `{ at_ms,
event_idx, rows, cols }` (a byte-log marker, not a grid dump). Seeking to T: binary
search the marker ≤ T, spin a fresh `Vt100Emulator`, re-feed the bounded byte slice
— exact by construction, **zero emulator-trait changes**. Bounded by bytes + time
(`[replay] max_bytes_per_pane=8MiB`, `max_duration_per_pane=30m`); eviction drops
front events and orphaned keyframes. `enabled=false` ⇒ `None`, one null check,
zero allocation. `Instant::now()` on the loop is a vDSO read — no wakeups added.
Optional `persist=true` mirrors the ring to `replay/<session>/<pane>.szr` on a
dedicated off-loop writer thread (diff-watcher pattern).

## Replay UI (Phase 2)

`ReplayOverlay` (sibling of `SearchOverlay`, captures all keys) paints from a
**scratch** emulator, never the live pane. Playback clock = a ticker thread that
exists **only while playing** (pulses `TerminalWaker`, parks on pause/exit) — an
event producer, not a poll, so idle stays 0 wakeups. Skip-inactivity collapses
gaps > `idle_threshold`. Time-search re-feeds frames and tests the ANSI-stripped
grid (`row_text`) so it finds strings that only ever appeared inside alt-screen
apps; runs on `spawn_blocking`, streamed back over a channel. Time expressions
(`1h30s`) via a `parse_duration` helper.

## Registers (Phase 3) + screen swap (Phase 4)

Registers (`registers.rs`, pure/coverage-gated) persisted in a new `registers`
table (DB **v16**, additive `CREATE TABLE IF NOT EXISTS`); `"+` = system clipboard
(not persisted). Alt/main screen swap ships via the replay-subsumed path; the live
toggle is deferred pending vt100 0.16 inactive-screen API.

## Invariants

Recorder is the only always-on addition (measured via `just bench`); the playback
clock is the only timer and is scoped to active playback; everything else rides
the waker. Tests isolate `XDG_STATE_HOME` and the replay dir.
