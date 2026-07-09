# Observe Time Range & Variables

## Purpose

Time range and variables control what every Observe query sees. A global time range (absolute or relative) with a quick-range picker and pausable auto-refresh drives re-queries and cancels in-flight ones on change. Keyboard time navigation zooms and pans focused time-series panels, and variables (query/custom/interval/constant/textbox) interpolate into query strings with dependency chaining and multi-value expansion.

## Requirements

### Requirement: Global time range with auto-refresh

[M] Observe SHALL have a global time range (absolute and relative, e.g. `now-1h`) with a quick-range picker and a configurable auto-refresh interval that can pause/resume; a time-range change MUST cancel in-flight queries and re-issue them.

#### Scenario: Pause auto-refresh

- **WHEN** the user pauses auto-refresh
- **THEN** no further automatic re-queries occur until resumed, and no refresh
  ticker remains active

### Requirement: Keyboard time navigation

[S] On a focused time-series panel the user SHALL be able to zoom into a sub-range, zoom out, and pan via the keyboard; [S] per-panel relative time-range overrides MUST be supported.

#### Scenario: Zoom into a sub-range

- **WHEN** the user selects a sub-range on a focused time-series panel
- **THEN** the global (or panel) range narrows to it and re-queries

### Requirement: Variables and templating

[S] Observe SHALL support variables (query-from-discovery, custom static list, interval, constant, textbox) interpolated into query strings before execution, with dependency chaining and multi-value/"all" expansion, surfaced in an interactive picker that re-queries on change.

#### Scenario: Change a variable re-queries

- **WHEN** the user changes a variable value in the picker
- **THEN** dependent queries are re-interpolated and re-executed
