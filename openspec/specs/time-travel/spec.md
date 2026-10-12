# time-travel Specification

## Purpose

Per-pane time-travel recording with an Alt+r scrub and search overlay, vim registers, and replay-subsumed screen swapping.

## Requirements

### Requirement: Bounded per-pane recording that is free when disabled

When `[replay] enabled` is true, thegn SHALL record each pane's byte stream as timestamped events with periodic keyframe markers, bounded by both a byte and a duration budget (evicting oldest events and orphaned keyframes); when disabled it MUST impose zero cost (no allocation, a single null check in `feed`).

#### Scenario: Disabled is free

- **WHEN** replay is disabled
- **THEN** no recording is allocated and `feed` does only a null check

#### Scenario: Budget eviction

- **WHEN** a pane's recording exceeds its byte or duration budget
- **THEN** oldest events are dropped, and any keyframe whose byte range no longer
  exists is dropped with them

### Requirement: Seek discloses history loss

When no history has been evicted or truncated, seeking to a time T SHALL
reconstruct the pane's grid by spinning up a fresh emulator and re-feeding the
recorded bytes up to T (forward playback may feed incrementally from the current
position), with no changes to the `PaneEmulator` trait. Once history has been
evicted or truncated, replay, time-search, and export MUST disclose that the
retained history is partial; they MUST NOT claim exact reconstruction.

#### Scenario: Seek with complete history

- **WHEN** the user seeks to time T and no history has been evicted or truncated
- **THEN** the reconstructed grid matches what the live emulator showed at T

#### Scenario: Seek after eviction discloses partial history

- **WHEN** the user seeks or searches after history has been evicted or truncated
- **THEN** the replay overlay and search result disclose partial history

### Requirement: Replay playback adds no idle wakeups

The replay overlay SHALL paint from a scratch emulator (never the live pane), and the playback clock MUST pulse the waker only while playing and park (zero CPU) on pause/exit, so a paused or closed replay adds zero wakeups.

#### Scenario: Paused replay is idle

- **WHEN** replay is paused or closed
- **THEN** the playback clock is parked and the loop receives no replay wakeups

### Requirement: Search across time finds any on-screen string

Time-search SHALL reconstruct frames over the recording and test grid text read cell-by-cell (styling-agnostic, not `row_text`, which returns nothing for a styled row) so it can find strings that only ever appeared inside full-screen apps. It is bounded by the recording budget and triggered by an explicit user action (the `/` prompt).

#### Scenario: Find text from an alt-screen app

- **WHEN** the user searches for a string that appeared only inside a full-screen
  app (never in scrollback)
- **THEN** time-search locates the frame where it appeared

### Requirement: Persisted named registers

thegn SHALL provide vim-style named registers (`"a`–`"z`, `"0`–`"9`, the default `"`, and the system-clipboard `"+`) stored in the state DB across restarts, except the volatile `"+` register which reads/writes the live OS clipboard and is never persisted. A copy SHALL populate the default register, and `PasteRegister` SHALL write a chosen register's value into the focused pane (honoring bracketed paste).

#### Scenario: Default register survives a restart

- **WHEN** the user copies text, then restarts thegn
- **THEN** pasting the default register (`PasteRegister` `"`) yields the copied text

#### Scenario: Clipboard register is not persisted

- **WHEN** registers are persisted to the DB
- **THEN** the `"+` register is excluded (it maps to the live system clipboard)

### Requirement: A pane's recording exports as an asciicast

thegn SHALL export the focused pane's retained time-travel recording as an
asciicast v2 file (header with the pane's geometry at the earliest retained
event, output events with times rebased to zero), triggered from the replay
overlay and a palette action, written under the per-profile recordings
directory with owner-only permissions. The export MUST be bounded by the
existing replay budget and MUST be honest about it: the destination path and
the covered timespan are reported to the user. When replay is disabled or the
ring is empty the action MUST fail with a clear message rather than writing
an empty file.

#### Scenario: Export from the replay overlay

- **WHEN** the user opens replay on a pane with retained history and invokes
  export
- **THEN** a `.cast` file is written under the recordings directory and a
  toast reports its path and the timespan it covers

#### Scenario: Replay disabled

- **WHEN** `[replay] enabled` is false and the user invokes the export action
- **THEN** no file is written and the message names the `[replay]` setting

#### Scenario: The export replays faithfully with complete history

- **WHEN** an exported cast with no evicted or truncated history is fed to an
  asciicast v2 player at the recorded geometry
- **THEN** it reproduces the pane's retained visual history

#### Scenario: Partial history is disclosed in an export

- **WHEN** the recording has evicted or truncated earlier history
- **THEN** the export includes a partial-history marker and the success toast
  says "partial"

### Requirement: Corner pins do not retain per-pane replay recordings

Replay and cast export SHALL use the focused center pane only. A corner pin MUST
not retain per-pane replay bytes, and the separate whole-session recorder MUST
remain unaffected.

#### Scenario: Replay and export with a corner pin

- **WHEN** a corner pin is active while replay or cast export is invoked
- **THEN** the action uses the focused center pane, and the corner pin has no
  per-pane replay history to open or export

#### Scenario: Whole-session recording with a corner pin

- **WHEN** whole-session recording is enabled while a corner pin is active
- **THEN** whole-session recording continues independently of per-pane replay
