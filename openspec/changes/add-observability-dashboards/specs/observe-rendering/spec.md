# Observe Rendering

## ADDED Requirements

### Requirement: Downsample before render bounds plot cost

[M] Charts SHALL downsample series to the target cell/pixel width (LTTB and/or min-max-per-bucket) before plotting, so plot cost is bounded regardless of series cardinality, with null-gap handling and common-time-axis alignment across series.

#### Scenario: High-cardinality result stays cheap

- **WHEN** a query returns far more points than the panel is wide
- **THEN** the series is downsampled to the panel width before rendering

### Requirement: Capability-detected renderer with graceful fallback

[M] At tile attach Observe SHALL detect terminal capabilities and select the best renderer, MUST always provide a braille/block fallback that works on any terminal, and MUST degrade truecolor → 256 → 16 → mono; [S] a graphics-protocol (sixel/kitty) renderer via `ratatui-image` is used when available.

#### Scenario: No graphics protocol available

- **WHEN** the terminal supports neither sixel nor kitty graphics
- **THEN** charts render via the braille/block fallback rather than failing

### Requirement: Rendering never blocks the event loop

[M] Query execution SHALL run off the event loop and deliver results over a channel that pulses the `TerminalWaker`; rendering MUST NOT block the loop on network I/O and MUST NOT introduce a polling timeout. [S] series colors are assigned by stable hashing of series identity.

#### Scenario: Slow query does not stall the UI

- **WHEN** a query is slow to return
- **THEN** the tile stays responsive and repaints when the result arrives
