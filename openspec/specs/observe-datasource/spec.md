# Observe Data Source

## Purpose

Data sources are the backend abstraction behind every Observe query. Each implements a common `DataSource` trait with capability flags, async cancellable queries, health checks, per-source config, pooled HTTP, and structured errors. Observe ships built-in Prometheus, Loki, a `host` source, and a synthetic test source so it renders data with no external configuration.

## Requirements

### Requirement: DataSource trait with capabilities and health

[M] Every backend SHALL implement a `DataSource` trait exposing `query(&Query, TimeRange) -> Vec<Frame>`, a `capabilities()` flag set (metrics/logs/traces/variables/streaming/label-discovery), and a `health_check()`; queries MUST be async and cancellable, with cancellation tied to time-range changes and panel teardown.

#### Scenario: Time-range change cancels in-flight queries

- **WHEN** the global time range changes while a query is running
- **THEN** the in-flight query is cancelled and re-issued for the new range

#### Scenario: Capability gates a feature

- **WHEN** a source does not advertise streaming
- **THEN** streaming/tail UI is not offered for that source

### Requirement: Per-source config, pooling, and structured errors

[M] Each source SHALL carry its own config (URL, auth bearer/basic/header/none, TLS opts, timeout, custom headers) with a reused pooled HTTP client, and errors MUST be surfaced as a structured set distinguishing network / auth / query-syntax / timeout / partial-result.

#### Scenario: Auth failure is distinguishable

- **WHEN** a source returns 401/403
- **THEN** the error is classified as auth, not a generic failure

### Requirement: Built-in sources

[M] Observe SHALL ship Prometheus (instant + range, step derived from panel width), Loki (LogQL range + tail), a `host` source backed by `thegn-metrics`, and a synthetic test source; [S] a generic SQL source via `sqlx`; [S] result caching keyed by (source, query, range, step) with TTL and [S] label/metric discovery for autocomplete.

#### Scenario: First run with no backend

- **WHEN** Observe starts with no configured external source
- **THEN** the built-in test and `host` sources still render data
