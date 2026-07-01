# Tasks

## Phase 0 — walking skeleton

- [x] 0.1 `gtui-core`: `Frame`/`Field` model (typed columnar on Polars/Arrow,
      field + frame metadata, time semantics, wide⇄long) — **unit tests** (95% gate).
- [x] 0.2 `gtui-core`: `DataSource` trait + `Query`/`TimeRange`/`Caps`/`QueryError`.
- [x] 0.3 `gtui-query`: synthetic test source (random-walk) — **unit tests**.
- [x] 0.4 `gtui-render`: braille time-series + LTTB downsample + unit-aware axes —
      **unit tests** (downsample bounds, axis formatting).
- [x] 0.5 `gtui-app`: event loop + global time range; render off-loop via channel +
      waker (assert no polling timeout).

## Phase 1 — MVP [M]

- [x] 1.1 `gtui-query`: Prometheus (instant + range, step from panel width), Loki
      (LogQL range + tail), `host` source over `superzej-metrics` — fixture-tested parsers.
- [x] 1.2 Explore mode: split editor/viz, auto-detect viz, session history.
- [x] 1.3 Core panels: time-series, stat, table, logs + field config + legend toggle.
- [x] 1.4 Auto-refresh ticker (alive only while active + refresh>0; pause/resume);
      time-range change cancels in-flight queries — **test** cancellation + no idle ticker.
- [x] 1.5 Native TOML dashboard format + load-from-directory + command palette.
- [x] 1.6 `gtui-embed`: Observe app-tab via sz-kit + `catch_unwind` panic boundary —
      **test** a panicking panel degrades, host survives.

## Phase 2 — [S]

- [ ] 2.1 Transforms (reduce/filter/organize/join/reshape) on Polars — **unit tests**.
- [ ] 2.2 Variables/templating (query/custom/interval/constant) + interpolation.
- [ ] 2.3 In-TUI dashboard edit/save + file-watch reload; SQL source (`sqlx`);
      gauge/bar panels; query cache + discovery-backed autocomplete.

## Phase 3 — [C]

- [ ] 3.1 Graphics-protocol rendering (sixel/kitty via `ratatui-image`) with braille
      fallback + capability detection — **test** degrade path.
- [ ] 3.2 Heatmap, traces (Tempo waterfall), read-only alerts view, subprocess/WASM
      plugin sources.

## Validate

- [ ] V.1 Run `just ci` (includes `openspec-validate`).
