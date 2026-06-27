# Add observability dashboards ("Observe")

## Summary

Add a Grafana-compatible observability frontend to superzej as an **"Observe"
app-tab**: a keyboard-native, modal metrics/logs/traces client with two modes —
**Explore** (ad-hoc query REPL with viz) and **Dashboards** (persisted TOML
layouts) — backed by a query-backend-agnostic `DataSource` abstraction
(Prometheus, Loki, SQL, a built-in `host` source over `superzej-metrics`, and a
synthetic test source). It is **not** a query engine: it delegates execution to
backends and normalizes responses into a typed columnar `Frame` model.

This is the full product (Phases 0–3) specified in one change, with every
requirement priority-tagged **[M]** (v1 MVP) / **[S]** (v1.x) / **[C]** (later) /
**[W]** (non-goal). Rendering and embedding follow **Approach A**: ratatui 0.30 +
`ratatui-image`, composited through superzej's existing sz-kit `AppTile` contract.

## Impact

- **AH** (resource / system monitoring) — promotes the host metrics into a full
  dashboard surface; the `host` DataSource reuses `superzej-metrics`.
- **AM** (daily-driver / non-code tiles) — Observe is a first-class app-tab.
- **L** (statusbar widgets) — panel thresholds can feed statusbar chips later.
- Folds in the deprecated `.hermes` `dashboard-integration` and
  `prometheus-metrics-sidebar` plans.
- New capabilities: `observe-dataframe`, `observe-datasource`, `observe-explore`,
  `observe-panels`, `observe-rendering`, `observe-dashboards`,
  `observe-timerange-variables`, `observe-integration`.

## Rationale

superzej already runs a damage-region compositor, a metrics leaf crate, and an
sz-kit app-tab path for sibling TUIs (ratatui 0.30). An observability tile rides
all three: queries run off-loop and wake the loop on results (0%-idle preserved),
downsampling bounds plot cost regardless of cardinality, and the typed `Frame`
core stays substrate-agnostic and unit-testable behind the `DataSource` boundary.

## Non-goals (v1, **[W]**)

Alert rule evaluation; Grafana JSON export / round-trip; RBAC / multi-tenant;
plugin marketplace; geomap; server/hosted mode. A standalone single-file binary is
also out of scope — Observe ships inside `szhost` (though `gtui-*` crates stay
standalone-capable).
