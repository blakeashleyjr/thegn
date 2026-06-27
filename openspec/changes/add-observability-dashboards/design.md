# Design

## Crate layout (workspace members)

- **`gtui-core`** — substrate-agnostic (no tokio/ratatui/termwiz), 95% gated:
  the `Frame`/`Field` columnar model (Polars/Arrow), `DataSource` trait, `Query`/
  `TimeRange`/`Caps`, transform pipeline, dashboard serde model.
- **`gtui-query`** — async execution: tokio + a pooled `reqwest` client, cancellation
  tied to time-range change + panel teardown, structured `QueryError`, TTL result
  cache. Source impls: Prometheus, Loki, SQL (`sqlx`), `host` (over
  `superzej-metrics`), synthetic test source.
- **`gtui-render`** — ratatui 0.30 + `ratatui-image`; braille/block fallback,
  LTTB/min-max downsampling, unit-aware axes, terminal capability detection,
  graphics-protocol (sixel/kitty) renderer, stable series-color hashing.
- **`gtui-app`** — Explore + Dashboards modes, grid layout, focus model, command
  palette, time controls, variables.
- **`gtui-embed`** — sz-kit `AppTile` adapter; `superzej-host` gains the Observe tab.

## Rendering & the 0%-idle contract

Datasource queries run **off the event loop** (tokio); results return over a
channel and **pulse the `TerminalWaker`**, then the tile re-renders and is
composited by diff. **No blocking I/O on the loop; no polling timeout.**
Auto-refresh is a ticker thread that exists **only while an active dashboard has
refresh > 0** (the replay-clock / 2s-refresh pattern), parked on pause or
tab-switch. **Downsample-before-render** bounds plot cost regardless of series
cardinality; high cardinality caps to top-N with a warning; streaming uses ring
buffers with retention caps. Capability detection at tile attach picks
graphics-protocol → braille → block and truecolor → 256 → 16 → mono.

## superzej integration

Mounted as an **"Observe" app-tab** via sz-kit (ratatui 0.30 tile). The tile is
wrapped in a **`catch_unwind` boundary**: a panel render/query panic degrades that
panel (or the tile), never the host — superzej owns terminal restore. A built-in
**`host` DataSource** exposes `superzej-metrics` as frames for a zero-config
first-run view. Config layers into superzej TOML (`[observe]`, `[observe.source.
<name>]` with URL/auth/TLS/timeout); secrets via `env:`/file/keyring, never inline;
dashboards load from a directory under the superzej config tree.

## Data model

`Frame { fields: Vec<Field>, meta }`; `Field { name, ty, values: ArrayRef, config,
labels }`. First-class time semantics with wide⇄long conversion; zero/low-copy
slice + downsample. `DataSource::{query (async, cancellable), capabilities,
health_check, discover}`; `QueryError` distinguishes network/auth/syntax/timeout/
partial.

## Phasing (priority-tagged within this one change)

- **Phase 0** skeleton: `gtui-core`, `DataSource`, test source, braille
  time-series, global time range.
- **Phase 1 [M]**: Prometheus + Loki + host sources, Explore, the 4 core panels,
  auto-refresh, TOML dashboards, command palette, the Observe tab.
- **Phase 2 [S]**: transforms, variables, in-TUI dashboard edit, SQL source,
  gauge/bar, query cache + autocomplete.
- **Phase 3 [C]**: graphics-protocol rendering, heatmaps, traces (Tempo),
  read-only alerts view, subprocess/WASM plugin sources.

## Flagged risks

- Terminal **graphics detection/fallback** is isolated in `gtui-render`'s probe;
  braille is the always-works floor, so the feature never hard-depends on sixel.
- **Grafana JSON import** stays `[C]`, lossy, behind a flag (schema tar pit);
  export/round-trip is `[W]`.

## Invariants

0%-idle (off-loop queries + scoped refresh ticker, no polling); per-panel error
isolation; panic boundary with terminal restore; credentials redacted in logs and
the query inspector; TLS-verify default-on; `gtui-core` unit-tested at the 95%
gate; AI-free (this is an observability surface, no LLM dependency).
