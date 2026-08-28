# Security / test / bug review — THE-67 (drag grab precision)

**Reviewer:** security/test/bug lane.
**Branch:** `tg/the-67-drag-precision`, reviewed at the fresh `git merge main`
(commit `49242455` on top of the architect-reviewed `a9829c82`).
**Lane docs read:** architect/design.md (F1–F7 audit + drag model), chunk-1/2/3
specs + done artifacts, architect-review/verdict.md (APPROVED; its two fixes —
Esc Wide-expand restore, stale-hint clear — verified in the tree, run.rs
14185–14232 / 12758–12873).

## Verdict

PASS

(ready for the merge queue — `thegn integrate` is the lead's step, not run here)

## 1. Binding merge addendum — done first

`main` had moved well past the architect's merge (THE-72/73/74/75/85 all
landed — the exact sidebar/chrome churn the addenda warned about: 85 commits).
Merged clean, **zero textual conflicts** in `sidebar_view.rs`,
`sidebar_mouse.rs`, `chrome.rs`, `run.rs`; the pre-commit treefmt hook again
reformatted incoming THE-75 pipeline docs and refused the auto-commit, resolved
by committing as-is (`49242455`). Note: that merge commit is **unsigned** —
GPG signing timed out repeatedly in this harness (no pinentry); environmental,
not a repo-policy deviation. All geometry tests re-run green on the merged
tree (below).

## 2. Adversarial review of the full branch diff (`git diff main...HEAD`)

Scope: `drag_hit.rs` (new), `run.rs` (+290), `pane_drag.rs`, `chrome.rs`,
`handlers/sidebar_mouse.rs`, `sidebar_view.rs`, help pages, openspec change.

**Grab bands vs neighbouring controls (the lead's #1 risk) — safe, and pinned:**

- Separator bands take their extra cell from the **center frame cell only**
  (`SepSide::Sidebar` → `sep+1`, `SepSide::Panel` → `sep−1`), never from a list
  row; the drawer-at-`center_x` case is gated by
  `sep_is_exact(...) || hit_pane.is_none()` at both arms (run.rs 12897/12915).
  Dispatch order verified: grab arms (12876+) precede the sidebar row-press
  handler (13384), so the exact separator column still grabs rather than
  activating a row — identical to main's pre-branch 1-column behavior there.
- Tab-chip gap: `center_tab_hit` widens into the inter-chip spacing column but
  clamps at `strip_chip_end` — and placement + hit-test share that one source,
  so they cannot drift. The no-steal invariant is pinned by
  `center_tab_hit_widening_stops_at_the_pin_strip`: the last chip's gap cell
  resolves to the **pin** (`pin_chip_hit == Some(1)`), not the tab.
- Sidebar tail clamp: `row_at_clamped` only runs on the **drag** path
  (`spot_at`); press paths (`sidebar_mouse.rs` 205/310) keep strict `row_at`, so
  a click on blank space still never selects. The clamp is bounded inside the
  sidebar rect and its choice is re-validated by `spot_for_hover` — a clamp
  onto a foreign-workspace / terminal / pipeline / home-anchored row is
  `Spot::Invalid` (RowKind fall-through `_` arm), so the clamp converts
  "nothing" into "nearest row", never a wrong drop.

**Esc cancels in every surface:** sep-grab Esc block (14185) restores
`sb.width` snapshot, the Wide-expand flag **and writes it back**
(`sb.persist("sidebar_expanded", "1")` — matching `collapse_wide`'s persist of
"0"; `collapse_wide` touches only `expanded`+persist, so the restore is exact),
recomputes `sidebar_cols`, restores `panel_cols_pref` +
`set_panel_width_cfg` (in-memory atomics only — motion persists nothing, so
cancel correctly persists nothing), clears the press hint, and resets
`mouse_left_down`. Ordering vs the sidebar **row**-drag Esc cancel (14247) is
disjoint by guards; no double handling.

**Motionless releases move nothing:** sidebar/panel use the `mx == press_x`
threshold (a duplicate/jitter sample at the press column cannot collapse Wide
or mark `moved`) and a `moved == false` release persists nothing +
`model.status.clear()`; a motionless pane lift focuses the pane with the exact
content-click lines (`focus.zone = Center; tab.focused_pane = dragged`, run.rs
12721–12724 vs 13157–13160) and no commit. Motionless pane-border release still
persists the session layout — pre-existing, architect-accepted residual,
untouched here.

**No drag-state leak across tab switch / pane close:** grabs are loop-local and
cleared on release/Esc; a keyboard tab-switch or pane close mid-lift leaves
`dragged` stale, but `CenterTree::swap`/`anchor` guard
`positional_node_mut(...).is_none() → false` and `resolve_drop` never panics on
an absent id — a stale commit is a guarded no-op, no wrong-tree mutation.

**Degradation/width:** nothing painted changed — no color/glyph literals, no
`width()`-dependent geometry added; ascii-glyph widths cannot desync the
hit-tables because `strip_chip_end` is shared by painter and hit-test.

**Swallowed errors / injection / permission:** no new `let _ =` / `.ok()` on
the diff (only pre-existing sanctioned DB-cache persists inside `db_task`
closures); no new I/O, no paths, no subprocess, no forge CLIs. `sep_follow` is
saturating; widths clamp to `SIDEBAR_MIN_WIDTH`/`sidebar_max_width` and
`(cols/2).max(30)`.

**Ratchets:** no new platform `cfg`, `async fn` in traits, color/glyph
literals, or ignored Results; no new `ACTION_SPECS` ids — help ratchet
73/73 green; allowlist files untouched by the branch.

## 3. Scoped tests run (post-merge, merged tree)

- `cargo clippy -p thegn-host --tests -- -D warnings` — clean.
- `cargo nextest run -p thegn-host drag_hit center_tab row_at border_at
sidebar_mouse sep_grab sep_follow strip_chip` — **49/49**.
- `cargo nextest run -p thegn-host pane_drag:: chrome_tests:: sidebar_view::
layout::tests tabbar` — **82/82**.
- `cargo nextest run -p thegn-host help` — **73/73** (help ratchet incl.
  claim+prose checks).
- `openspec validate --all --strict` (pinned store binary) — **170/170**.
- No full-workspace gate run (dev-loop policy; pre-push/`just ci` own those).

## 4. Frames / e2e

No recorded baseline shifts with default config: the only muse spec using
mouse events is `22-glitch-hunt-input-routing.yaml`, clicking pane **content**
cells (col 50) that this change does not re-route; the press-time "drag to
resize" status hints already existed on main; `PANE_HPAD` defaults to 0 so the
seam slop is inert; hit-test-only changes (chip gap cell, separator band cell,
tail clamp) are on cells no spec clicks. Standing baseline staleness (last
recorded `0f9c5a9a`, per CLAUDE.md) is a separate pre-existing issue. The
change's tasks.md 4.3 pre-PR gate (`just ci` + deliberate `just e2e-update`
review) remains the documented open step for the lead.

## 5. Non-blocking notes (no action required for the queue)

1. **Sidebar marks not cleared by the new motionless title-bar focus** — a
   content click into a pane clears `sb.marked`, a title-bar click focuses
   without clearing. Cosmetic inconsistency on a path that previously did
   nothing at all; keyboard focus transitions don't clear marks either, so
   this matches the keyboard path. Follow-up polish at most.
2. **Separator drags are not in `pre_dispatch`'s pointer-capture flag** —
   verified identical on main (pre-existing, architect-accepted residual): a
   release landing inside a mouse-reporting pane app leaves the grab armed
   until a chrome-side release. Good follow-up-issue candidate; not widened by
   this branch.
3. **Merge commit `49242455` is unsigned** — GPG pinentry unavailable in this
   harness (signing timed out); all other commits retain their signatures.

## 6. Checklist closure

Every coder "Unverified" item and architect follow-up was individually
addressed above: openspec-recipe-vs-pinned-binary (ran the pinned binary:
170/170), live-TUI drag feel (unit-verified only, per lead's no-e2e — carried
as residual), rect-boundary coverage (both edges tested), full-workspace gates
(deliberately not run here — pre-push/`just ci` policy).
