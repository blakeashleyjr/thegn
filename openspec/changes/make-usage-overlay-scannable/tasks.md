# Tasks — scannable usage overlay

## 1. Pure presentation logic

- [x] 1.1 Extract a unit-testable section builder: (accounts, history, tokens,
      expanded set) → sections; compact one-row-per-account default using
      `peak_window()`; worst-first sort with loading/unavailable last.
      Shipped as `thegn_core::usage_view::build` (worst-first, `peak_only`
      compact mode) projected by `usage_sections` in
      `crates/thegn-host/src/detail/usage_dash.rs`. The per-account expanded
      set was not built (see 2.1).
- [x] 1.2 Tests: sort order, compact row shape, expansion round-trip,
      loading/unavailable placement, token block collapsed by default.
      Shipped subset: `order_is_worst_first_across_states_and_keeps_discovery_order`
      and `build_peak_only_takes_the_peak_window_not_the_first`
      (`usage_view.rs`); `worst_account_leads_with_a_loud_label_and_unavailable_sinks`
      (`usage_dash.rs`). Expansion round-trip and collapsed token block are
      not covered (no expansion state exists).

## 2. Overlay + panel wiring (thegn-host)

- [ ] 2.1 Expansion state on the usage overlay (beside `sel`); toggle key per
      the detail-overlay convention; aligned gauge column; abbreviated home in
      compact, full in expanded facts.
      NOT shipped: the overlay always shows every window (`peak_only: false`,
      `usage_dash.rs`); no expansion state or toggle key.
- [x] 2.2 System ▸ Usage panel section reuses the shared compact row builder.
      Shipped: `panel/sections/usage.rs` calls `usage_view::build` with
      `peak_only: !ctx.deep()`.

## 3. Snapshot verb

<!-- Open: capability-surface work (scoped `usage.snapshot` read surface,
     `thegn usage --json` from cached state, MCP listing). -->

- [ ] 3.1 `usage.snapshot` capability row (Read): Verb + `required_scope` arm +
      `cap(...)` + `ControlApi` method + HTTP route; gRPC mirror or recorded
      `SURFACE_GAPS` entry; catalog ratchet tests green.
- [x] 3.2 `thegn usage [--json] [--tokens]`: plain aligned table default, full
      payload JSON via the one-emitter convention; gather runs via
      `thegn_svc::usage::gather` (no live fetch flag). Done in
      `crates/thegn-host/src/cmd/usage.rs` (b150).
- [ ] 3.3 MCP: the snapshot tool lists under read scope on the write-tools
      branch's gating.

## 4. Docs + gates

- [ ] 4.1 Update `docs/help/ai-usage.md`: compact/expanded views, the toggle
      key, the CLI verb (help ratchet: new action ids claimed and mentioned).
- [ ] 4.2 If the overlay's frame changes are visible to muse baselines,
      re-record affected e2e snapshots with `just e2e-update` (review diffs).
- [ ] 4.3 Run `just ci` once at the end (includes openspec validate).
