# Time Travel

## ADDED Requirements

### Requirement: Bounded per-pane recording that is free when disabled

When `[replay] enabled` is true, thegn SHALL record each pane's byte stream as timestamped events with periodic keyframe markers, bounded by both a byte and a duration budget (evicting oldest events and orphaned keyframes); when disabled it MUST impose zero cost (no allocation, a single null check in `feed`).

#### Scenario: Disabled is free

- **WHEN** replay is disabled
- **THEN** no recording is allocated and `feed` does only a null check

#### Scenario: Budget eviction

- **WHEN** a pane's recording exceeds its byte or duration budget
- **THEN** oldest events are dropped, and any keyframe whose byte range no longer
  exists is dropped with them

### Requirement: Seek reconstructs the exact grid by re-feeding

Seeking to a time T SHALL reconstruct the pane's grid by spinning up a fresh emulator and re-feeding the retained byte slice up to T (forward playback may feed incrementally from the current position), with no changes to the `PaneEmulator` trait.

#### Scenario: Seek determinism

- **WHEN** the user seeks to time T
- **THEN** the reconstructed grid matches what the live emulator showed at T

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
