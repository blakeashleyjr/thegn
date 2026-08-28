# THE-75 — security/test/bug review verdict

Branch `tg/the-75-monitor-fixes`, reviewed at `0d73161e` (post-merge of `main`
at `6040fa1d`). Lane docs read: architect `design.md`, chunks 1–3 + done
reports, architect-review `verdict.md` (APPROVED, 2 corrections) — its
follow-ups and every coder "Unverified" item were re-checked here.

## Verdict

**PASS**

Ready for the merge queue (`thegn integrate` — not run here). Full per-crate
suites green: `cargo nextest run -p thegn-host` **2511/2511**, `-p thegn-core`
**3468/3468**; `just quick thegn-host` / `just quick thegn-core` (clippy
`-D warnings`) clean; help + glyph/color/platform ratchets green.

## 0. Lead addendum — merge of main (64b7bb9f)

`main` had moved again (THE-73 sidebar reap, THE-74 standalone pipeline board,
THE-85 landed after). THE-74 **removed the monitor's Pipeline tab** and made
the board its own surface, which conflicts with THE-75's chunk-3 board work.
Resolution, applied deliberately per file (not wholesale):

- **Dropped as superseded by THE-74's board** (nothing re-adds the tab, and no
  dead code remains — verified by grep across the tree):
  `MonitorTab::Pipeline` + its `ALL`/label/key/`widget_id` entries,
  `MonitorOverlay::pipeline_rows` + the rebuild fold + `TabInput` fields,
  `goto_tab` (its only caller was the Pipeline arm), `wants_dispatches`,
  `pipeline_key`, the footer's Pipeline arm, `MonitorAction::Pipeline` +
  `PipelineJump`, `monitor_action::pipeline_landing`/`PipelineLanding`
  (main's board has its own two-tier `pipeline_board::pipeline_target` — live
  sidebar row first, then `sidebar_db_worktrees` — flowing through
  `activate_row_target` with the hydration kick in `land_pipeline_jump!`, which
  is THE-75's chunk-3 behaviour carried by THE-74's design), and the monitor's
  whole Pipeline builder section in `build.rs`.
- **Kept THE-75's work, re-based onto the 9-tab world**: `tabbar.rs` digits +
  windowing, `TableSection.sel` + gutter + `S::Panel2` cursor background,
  `row_y`/`follow` viewport-chasing, the `?`/F1 help door
  (`MonitorOutcome::Help` + `overlay:monitor` context), footer gating,
  `procs_disabled`, `glyph_token` in core.
- **Kept both vocabularies in core**: THE-75's `glyph_token` (CLI's `glyph()`)
  and THE-74's `glyph_set` (chrome) — and fixed their one disagreement (§2).
- **Test surgery**: board-tab tests deleted (covered by main's
  `pipeline_board` suite, 36 tests incl. empty-stage org chart and the ASCII
  ladder); the `goto_tab`-based bar test now walks the digit keys; the
  tenth-tab test became `the_last_tab_is_reachable_by_its_digit` + a new
  `zero_beyond_the_visible_tabs_is_a_no_op` failure-path test (`0`'s mapping
  itself stays pinned in `tabbar.rs::digits_cover_ten_tabs_and_stop`). The
  headline `scrolling_never_retargets_the_destructive_key` (Processes `x`
  SIGTERM + Disk `x` clean, cursor and viewport proven in lockstep) survived
  the merge untouched and passes.
- **Fixed a main-side defect found in the merge**: main's committed
  `docs/help/system-monitor.md` contains a leftover diff3 marker
  (`||||||| 982ab7cb`, line 156) plus ~47 lines of stale pre-THE-74 board
  prose embedded in the shipped F1 page (an unresolved `diff3` conflict from
  THE-74's `879d6f89` merge). The merged tree repairs it: the section is now
  the pointer paragraph only. Flag for main: other merge commits may carry the
  same kind of marker; a marker-scan of `docs/help/` on main is cheap and worth
  doing.

## 1. Findings

### Fixed during this review

1. **`glyph_set`/`glyph_token` disagreed on `Unknown`** (drift introduced by
   the two-branch history): `glyph_token` (what `thegn dispatch list` prints)
   maps `Unknown → DotHollow`; main's `glyph_set` (board + sidebar) lumped
   `Unknown` with `Queued → diamond_hollow`. The CLI and the chrome could tell
   different stories about the same row — the exact failure both vocabularies
   exist to prevent. Fixed `Unknown → dot_hollow` (hue Blue kept) and added
   `dispatch_status_glyph_set_agrees_with_glyph_token_at_every_rung`, which
   pins every variant at both rungs so the two can never drift again
   (`0d73161e`).
2. **The stale `||||||| 982ab7cb` marker + dead prose in main's help page**
   (see §0) — repaired in the merge commit.
3. Merge-resolution compile fixes that also had review substance: `nav()`'s
   THE-75 version (cursor-only + `follow` re-arm) was restored where the
   splice initially lost its body — this is the destruct-key safety fix and
   its test covers it; `owner_tone` was restored to `build.rs` (Processes
   owner tint) — the deleted Pipeline section had contained it.

### Checked and clean (adversarial pass)

- **Destructive-key retargeting**: on the merged tree, list-tab navigation
  moves only the cursor (`nav`), the viewport follows via `follow_row` after
  `clamp()`, `wheel()` detaches (user took the viewport) and any cursor key
  re-arms. `x` (Processes signal, Disk clean) and the container lifecycle keys
  resolve `*[rows][sel]`, and the confirm prompt names that row — same row by
  construction, proven by `scrolling_never_retargets_the_destructive_key`.
  Confirm sub-mode owns every key while pending, so `?` cannot bypass a y/n.
- **Digits + `0`**: one mapping table (`tabbar::digit`/`index_of`, inverse by
  construction), out-of-range is a no-op returning `Pending` (no wrap-around,
  no panic — tested), tab switches reset `sel = 0` and re-arm `follow`.
- **Enter/hydration**: the merged tree's board Enter goes through
  `activate_row_target` (the sidebar's own door) and kicks the coalesced
  `model_refresh_pending` hydration on landing; no blocking work, no new wake
  source (main's landed design; the THE-75 loop arm it replaced is gone).
- **Bounds/overflow**: `follow_row` reads `scroll_max`/`body_rows` before the
  mutable borrow and min-clamps after; `row_ys` measures with
  `sections::stack_height` (same measure as `scroll_max`); `Section::height`
  for `Table` is header+rows — the gutter is horizontal only, and every
  non-monitor `TableSection` site got `sel: None` (frame-identical: no gutter,
  `table_cols` numerically unchanged for `None`).
- **No swallowed errors**: no new `let _ =`/`.ok()`/`unwrap`/`expect` in
  production code on the branch diff (all are in test code); the two
  best-effort sites predate the lane.
- **Ratchets**: no new glyph/color literal at a draw site (`tab_bar` uses
  `caps::glyph(QuoteOpen/QuoteClose)`, cursor uses `active_glyphs().half_block_r`,
  `S::Panel2` slot); help ratchets green and unmodified; `overlay:monitor` is
  claimed by the page that defines it.
- **No `async fn` in provider traits, no platform `#[cfg]` outside
  `platform/`, no `gh` outside the forge impl** (ratchet tests green).

## 2. Flags — no action required for this lane

1. **e2e / frame-affecting changes.** This lane changes monitor frames (tab
   bar digits + window markers, cursor gutters, footer hint order, help page),
   but **no muse spec drives the monitor overlay** (`grep -ri monitor
test/muse/specs/` → nothing), and the detail-table `sel: None` additions
   render identically — so **none of the 45 existing snapshots flips**; the
   lane's "all 45 stale" note was the pre-merge estimate. The monitor remains
   a frame-affecting surface e2e has never covered: when the gate is revived,
   add monitor specs (bar with digits on 80 cols, a list tab with the cursor
   on screen, per-tab footers) and re-record with `just e2e-update` then.
2. **Pre-existing on main, out of scope**: `hm_module_drift` clippy warnings
   (test target only, not in `just quick`); the `⏸ paused` literal in the
   monitor tab bar still bypasses the caps ladder (needs a `Glyph::Pause`
   token — same ratchet-window limit the architect recorded); a config stage
   literally named `unstaged` still draws twice on the board.
3. **`switch_workspace` opens/reads the state DB inline on the event loop**
   (run.rs:2010, `Db::open()` inside `activate_row_target`'s Workspace arm) —
   THE-73's landed design, exercised by every sidebar activation. The board's
   dormant-worktree Enter inherits it. Not this branch's diff; noted for a
   future off-loop pass.
4. **Commits on this branch are unsigned** — the signing key's passphrase is
   not reachable from this headless agent session (pinentry-gnome3 times out;
   `--pinentry-mode loopback` cannot prompt either). The merge and the two
   review commits are `--no-gpg-sign`; fold-actor's merges on this branch are
   unsigned already. Re-signing is unnecessary (git cannot rewrite a merge
   signature in place anyway); flagged for awareness only.
5. **Full-workspace gates remain outstanding by policy** (`just test`,
   `just coverage`, `just ci` — the Lead's pre-PR run; CI is dispatch-only).
   The per-crate full suites run here (2511 + 3468 tests) cover every touched
   crate but not the untouched ones; `just smoke` is the pre-push gate.

## 3. Verification run by the reviewer

- `git merge main` — resolved as §0; tree compiles; `just quick` both crates
  clean; treefmt clean (pre-commit hook green).
- `cargo nextest run -p thegn-host` — **2511 passed, 0 failed** (includes all
  monitor/tabbar/footer/sections/help/ratchet/pipeline_board/sidebar tests).
- `cargo nextest run -p thegn-core` — **3468 passed, 0 failed** (includes the
  new vocabulary-alignment test).
- `cargo clippy -p thegn-host --tests` / `-p thegn-core --tests` — clean
  except the pre-existing `hm_module_drift` warnings (main, not this lane).
- `git diff main...HEAD` reviewed in full (`crates/` + `docs/help/`); chunk
  done-report "Unverified" items dispositioned per §1/§2 above.
