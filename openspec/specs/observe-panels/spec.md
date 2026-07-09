# Observe Panels

## Purpose

Panels are the visualization units of an Observe dashboard. Observe ships a core set (time-series, stat, table, logs) plus optional gauge/bar and heatmap/trace types, each defined as query refs plus a transform pipeline, viz type, field config, and grid position. Field config drives units, thresholds, and colors; the legend is interactive; and a failing or slow source degrades only its own panel.

## Requirements

### Requirement: Core panel set

[M] Observe SHALL provide time-series (line/area/points, multi-series, legend), stat (single value + sparkline + threshold coloring), table (paged, sortable, cell formatting), and logs (streaming-capable, level coloring, label display, wrap toggle, filter) panels; [S] gauge / bar-gauge and bar chart; [C] heatmap, state timeline, and trace waterfall. Geomap is [W] (non-goal).

#### Scenario: Logs panel streams with level coloring

- **WHEN** a logs panel receives streaming lines
- **THEN** lines append with level coloring and label display without unbounded
  memory growth

### Requirement: Per-panel field config and interactive legend

[M] A panel SHALL be `(query refs) + (transform pipeline) + (viz type) + (field config) + (grid position)`, with field config covering units, decimals, thresholds, color mode, min/max, legend placement, and null handling; the legend MUST show per-series values (last/min/max/mean) and support key-to-toggle a series.

#### Scenario: Toggle a series from the legend

- **WHEN** the user toggles a series in the legend
- **THEN** that series is hidden/shown and the plot re-renders accordingly

### Requirement: A failing panel is isolated

[M] A failing or slow data source SHALL degrade only its own panel (a per-panel error state) and MUST NOT take down the tile or other panels.

#### Scenario: One panel's source is down

- **WHEN** one panel's source times out
- **THEN** that panel shows an error state while the others render normally
