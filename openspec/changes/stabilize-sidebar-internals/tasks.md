# Tasks

## 1. Spec reconciliation

- [x] 1.1 Amend + archive `add-sidebar-attention-sort` (reason-hint
      requirement deleted to match d9384f0b; task 8.3 re-scoped here).

## 2. Extractions (ratchet)

- [x] 2.1 `sidebar_view.rs`: the whole sidebar layout/paint block out of
      chrome.rs (placements, build/clamp, row/rail composers, metrics, row
      menu) + the `menu_rect` scroll-aware anchoring fix.
- [x] 2.2 `handlers/sidebar_keys.rs` + `handlers/sidebar_persist.rs`:
      SidebarState interaction + persistence out of run.rs (re-exported so
      call sites read unchanged); persisted-key inventory documented.

## 3. Internals

- [x] 3.1 SidebarRow: dead `agent` field removed, stale allows dropped,
      `base()` constructor collapses the eight field-by-field literals.
- [x] 3.2 Tombstone-free persistence: delete-not-zero on unpin/uncollapse +
      lazy legacy sweep + `activity` rewrite; `del_ui_state_prefix` (core, w/
      LIKE-escape) pruning on workspace/worktree/folder removal.
- [x] 3.3 `SortMode::default()` → Manual; unknown strings parse to default;
      round-trip test replaces the cycle test.
- [x] 3.4 GlyphSet: 14 new BMP width-1 fields with ASCII fallbacks; emoji
      dropped from chrome; ssh/mosh merged into one remote glyph; shared
      `MqStatus::glyph` for sidebar chip + panel section; width-1 + ASCII
      purity invariant tests.
- [x] 3.5 Rail: workspace initial, terminal dot+initial, EmptyHint blank.
- [x] 3.6 `[ui] sidebar_terminals_section` (config_enum, ViewState thread,
      build_rows gate, config.toml.example docs).

## 4. Tests

- [x] 4.1 ASCII-degradation render test (full + rail) — retires the
      missing-fallback bug class.
- [x] 4.2 Live/dormant pin-parity; terminals-section visibility; rail
      identity render test.
- [x] 4.3 core: `del_ui_state_prefix` + glyph invariants + TerminalsSection
      round-trip (coverage-gated).

## 5. Validation

- [x] 5.1 Ratchet re-locked (run.rs 18770→18560). Gates run individually
      (fmt-check, lint, build, test, smoke, openspec-validate, coverage
      core≥95%+proxy≥88%, doc-check — all green). Follow-up fixes landed for
      the formerly-red gates: `check-cross` (statvfs u32/u64 widening in
      thegn-metrics), `deps-audit` (advisory DB → target/advisory-dbs +
      machete ignore for generated-code prost), and the termwiz `?1003h`
      any-motion push (corrective DECSET write; mouse fully off on mouse-less
      terminals, button+drag only elsewhere).
- [ ] 5.3 e2e determinism mode (follow-up): the muse suite now normalizes
      volatile stats (net/disk/temp/clock/percents) suite-wide and runs at
      workers 2 / 20s deadlines, but ~half the cases still flake run-to-run
      on REAL app state — activity dots decaying ●→○ on quiet timers, the ✋
      needs-you chip appearing when a fresh shell settles (shifting the whole
      statusbar), and captures landing before the pane frame paints. Going
      green needs an app-side e2e env that freezes activity/chip/stats
      (regex normalization cannot mask semantic state).
- [ ] 5.2 Live TUI pass (`just start name=dev`): ASCII under `TERM=linux`,
      rail identity, terminals-section toggle, Manual default order.
