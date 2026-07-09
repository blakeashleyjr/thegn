# Tasks

## 1. Core scoring (superzej-core)

- [x] 1.1 `attention.rs`: `AttentionTier`/`AttentionReason`/`AttentionScore`,
      `score()`, `stable_order()` hysteresis, `next_attention()` wrap,
      `rollup()` — full tier-matrix unit tests (coverage-gated).
- [x] 1.2 `activity::read_entries()` — path-keyed accessor exposing
      `quiet_since`/`busy_since` (tests against a temp snapshot).
- [x] 1.3 `CacheStore::list_pr_cache()` (trait + `db_cache.rs` impl + test;
      db.rs untouched).

## 2. Hydration (superzej-host)

- [x] 2.1 `attention_status.rs`: join notifications/PR/MQ/CI/activity/dirty →
      per-path scores + hysteresis-stable ranks + workspace rollups; wired at
      the end of `collect_sidebar_status`. In-memory-DB tests.
- [x] 2.2 `SidebarStatus` gains `attention`/`attention_ranks`/
      `workspace_attention` (PartialEq for repaint gating).

## 3. Sort integration

- [x] 3.1 `SortMode::Activity` → `Attention`; `"activity"` parses as Attention;
      default = Attention; cycle unchanged; tests (migration, default,
      rank-order, no-ranks degradation).
- [x] 3.2 `sort_groups` Attention arm orders by rank (home/gi fallback);
      `Group.path`; `SidebarRow.attention` denormalized per row.

## 4. Workspace bubbling

- [x] 4.1 `[ui] sidebar_workspace_sort` config_enum in new `config_ui.rs`
      (UiConfig extracted from pinned config.rs); documented in
      config.toml.example; toml round-trip tests.
- [x] 4.2 `build_rows` stable tier sort of workspaces when enabled; wired from
      config at startup + live reload via `ViewState.workspace_sort`.

## 5. Reason hint

- [x] 5.1 `sidebar_legend::push_attention_reason` (caps-aware glyph + label,
      hue by tier, silent when idle) called from `compose_detail_line`; tests.

## 6. Jump-to-next key

- [x] 6.1 `GlyphSet.attention` (✋ / ASCII `!`).
- [x] 6.2 `Action::JumpAttention`, id `attention-next`, default `Alt a`
      (collision-free); ACTION_SPECS extracted to `keymap_specs.rs` (ratchet).
- [x] 6.3 `handlers/attention.rs` (`needs_user_ordered` + `next_target`) +
      run.rs dispatch arm; `activate_row_target` extracted to
      `handlers/sidebar_activate.rs` (ratchet).

## 7. Statusbar chip

- [x] 7.1 `statusbar_badges.rs`: CI/MQ chips extracted from chrome.rs + new
      `push_attention_badge` (`✋ N`, red when T0/T1 present); tests.
- [x] 7.2 `BarBadge::Attention` drill-down in `detail.rs` (rows: branch —
      reason — age; Enter focuses the worktree).

## 8. Validation

- [x] 8.1 `test/file-size-ratchet.sh --update` after the four extractions
      (all pinned files net-shrank).
- [x] 8.2 Run `just ci` once at the end (pre-PR gate). All gates green except
      two environmental/pre-existing issues, both verified NOT regressions:
      `deps-audit` (sandbox RO `~/.cargo` blocks the advisory-db lock; cargo-
      deny passes with a writable CARGO_HOME shim) and `e2e` (untracked
      `snapshots/` baselines are stale June-19 dashboard-era frames — fails
      identically on clean HEAD, confirmed via stash+rebuild).
- [ ] 8.3 Live check via `just start name=dev`: ordering, `Alt a`, chip,
      reason hints; confirm PR/CI ticking does not reshuffle rows. (Needs a
      real interactive session with agent/PR signals — pending restart into
      the new build.)
