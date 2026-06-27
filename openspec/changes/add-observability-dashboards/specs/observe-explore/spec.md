# Observe Explore Mode

## ADDED Requirements

### Requirement: Split query/result Explore view

[M] Explore mode SHALL present a query editor pane and a result viz pane, and MUST auto-detect the result frame shape to pick a default visualization (time series vs table vs logs).

#### Scenario: Log result picks the logs viz

- **WHEN** a query returns log-line frames
- **THEN** Explore defaults to the logs panel rather than a chart

### Requirement: Query history and stacked queries

[M] Explore SHALL keep query history within the session; [S] history MUST persist across sessions, [S] multiple stacked queries (A/B/C) with per-query show/hide MUST be supported, and [S] syntax-aware autocomplete SHALL use source discovery.

#### Scenario: Toggle a stacked query

- **WHEN** the user hides query B among stacked A/B/C
- **THEN** only the visible queries are plotted
