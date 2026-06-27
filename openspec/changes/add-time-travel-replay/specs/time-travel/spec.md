# Time Travel

## ADDED Requirements

### Requirement: Bounded per-pane recording that is free when disabled

When `[replay] enabled` is true, superzej SHALL record each pane's byte stream as timestamped events with periodic keyframe markers, bounded by both a byte and a duration budget (evicting oldest events and orphaned keyframes); when disabled it MUST impose zero cost (no allocation, a single null check in `feed`).

#### Scenario: Disabled is free

- **WHEN** replay is disabled
- **THEN** no recording is allocated and `feed` does only a null check

#### Scenario: Budget eviction

- **WHEN** a pane's recording exceeds its byte or duration budget
- **THEN** oldest events are dropped, and any keyframe whose byte range no longer
  exists is dropped with them

### Requirement: Seek reconstructs the exact grid by re-feeding

Seeking to a time T SHALL reconstruct the pane's grid by spinning up a fresh emulator and re-feeding the bounded byte slice from the preceding keyframe, with no changes to the `PaneEmulator` trait.

#### Scenario: Seek determinism

- **WHEN** the user seeks to time T
- **THEN** the reconstructed grid matches what the live emulator showed at T

### Requirement: Replay playback adds no idle wakeups

The replay overlay SHALL paint from a scratch emulator (never the live pane), and the playback clock MUST be a ticker that exists only while playing and parks on pause/exit, so a paused or closed replay adds zero wakeups.

#### Scenario: Paused replay is idle

- **WHEN** replay is paused or closed
- **THEN** no ticker thread is active and the loop receives no replay wakeups

### Requirement: Search across time finds any on-screen string

Time-search SHALL reconstruct frames over the recording and test the ANSI-stripped grid so it can find strings that only ever appeared inside full-screen apps, running off the loop with results streamed back over a channel.

#### Scenario: Find text from an alt-screen app

- **WHEN** the user searches for a string that appeared only inside a full-screen
  app (never in scrollback)
- **THEN** time-search locates the frame where it appeared
