# Monitor framework: tiled board, deep sampler, tiered history

Linear: THE-829

## Why

The system monitor is currently a vertical `Vec<Section>` stack. At a 240×60
terminal it fills only about 0.3 of the available overlay, while graph histories
are rebuilt as braille strings every frame. This change defines a tiled board,
a bounded sampling lifecycle, and reusable history so the interface can use its
space and stay responsive without adding an event-loop wake source. The
architecture and shared-reader ownership are summarized in this change's
`design.md`.

## What Changes

- Replace the vertical section stack with a deterministic tiled board that fills
  the overlay and degrades panels by priority when space is limited.
- Remove absent panels before layout and distinguish warming and denied sources
  without presenting missing data as zero.
- Add a deep `MonitorSampler` that exists only while the monitor is open and
  parks while hidden or paused, while the small base `StatsSampler` remains
  unchanged.
- Define bounded T0/T1/T2 history tiers, honest gaps, and a fixed memory budget.
- Set a measurable frame budget and compute graph rasters only when repainting.
- Make Overview the first tab, preserve a user's last tab by slug, and use one
  key scheme across monitor tabs.
- Provide per-tab help stubs and update the system-monitor help page for shipped
  behaviour.

This change records the framework contract; implementation is split across
F1a–F9b, SR-1–SR-6, and cleanup tasks in `tasks.md`.

## Non-goals

Per-tab collectors and views belong to later `monitor-<tab>` changes. macOS and
Windows collectors are not added here. T2 history is not persisted across
restarts. This change adds no action identifiers, configuration keys, or SQLite
schema changes.

## Impact

Affects the `system-monitor` capability and help context `overlay:monitor` at
`docs/help/system-monitor.md`. Tracks roadmap group AH items 411 and 416 in
`tasks.md`. Linear THE-829 coordinates framework work; sibling programme issues
THE-830 through THE-837 own implementation slices.
