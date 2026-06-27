# Observe Data Frame

## ADDED Requirements

### Requirement: Typed columnar frame model

[M] Observe SHALL represent all query results as columnar typed frames — named fields, each a typed null-aware array (f64/i64/time/string/bool) built on Arrow/Polars rather than hand-rolled — with per-field metadata (type, unit, display name, labels, config overrides) and frame-level metadata (source id, query ref, execution time, row count, notices).

#### Scenario: Backend response becomes a frame

- **WHEN** a data source returns results
- **THEN** they are normalized into one or more typed frames carrying field and
  frame metadata

### Requirement: First-class time semantics with wide/long conversion

[M] A frame MAY designate a time field, and Observe SHALL support both wide (one time column + N value columns) and long (label-keyed) representations with conversion between them.

#### Scenario: Convert long to wide

- **WHEN** a long-form frame is requested in wide form
- **THEN** it is converted to one time column plus per-series value columns

### Requirement: Low-copy slicing and downsampling

[S] Frames SHALL support zero/low-copy slicing and downsampling, and [S] incremental diffing so streaming appends do not force a full re-render.

#### Scenario: Downsample a frame

- **WHEN** a frame is downsampled to a target width
- **THEN** the operation avoids copying the underlying columns where possible
