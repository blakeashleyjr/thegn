# Tasks — scannable usage overlay

## 1. Pure presentation logic

- [ ] 1.1 Extract a unit-testable section builder: (accounts, history, tokens,
      expanded set) → sections; compact one-row-per-account default using
      `peak_window()`; worst-first sort with loading/unavailable last.
- [ ] 1.2 Tests: sort order, compact row shape, expansion round-trip,
      loading/unavailable placement, token block collapsed by default.

## 2. Overlay + panel wiring (thegn-host)

- [ ] 2.1 Expansion state on the usage overlay (beside `sel`); toggle key per
      the detail-overlay convention; aligned gauge column; abbreviated home in
      compact, full in expanded facts.
- [ ] 2.2 System ▸ Usage panel section reuses the shared compact row builder.

## 3. Snapshot verb

- [ ] 3.1 `usage.snapshot` capability row (Read): Verb + `required_scope` arm +
      `cap(...)` + `ControlApi` method + HTTP route; gRPC mirror or recorded
      `SURFACE_GAPS` entry; catalog ratchet tests green.
- [ ] 3.2 `thegn usage [--json] [--tokens]`: plain aligned table default, full
      payload JSON via the one-emitter convention; gather runs via
      `thegn_svc::usage::gather` (no live fetch flag).
- [ ] 3.3 MCP: the snapshot tool lists under read scope on the write-tools
      branch's gating.

## 4. Docs + gates

- [ ] 4.1 Update `docs/help/ai-usage.md`: compact/expanded views, the toggle
      key, the CLI verb (help ratchet: new action ids claimed and mentioned).
- [ ] 4.2 If the overlay's frame changes are visible to muse baselines,
      re-record affected e2e snapshots with `just e2e-update` (review diffs).
- [ ] 4.3 Run `just ci` once at the end (includes openspec validate).
