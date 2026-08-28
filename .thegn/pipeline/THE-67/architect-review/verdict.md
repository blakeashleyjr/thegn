# Architect review — THE-67 (drag grab precision)

**Reviewer:** architect pass, post-implementation.
**Branch:** `tg/the-67-drag-precision` · reviewed at `a9829c82` (after the
binding `git merge main` — THE-70/83/skills folded in cleanly, treefmt-gated,
committed `ec82648d`).
**Lane:** `.thegn/pipeline/THE-67/` (design.md, code/chunk-1..3.md + done).

## Verdict: APPROVED

(with two review fixes applied by the reviewer, commit `a9829c82`; no
revision chunk required)

---

## 1. Merge addendum

`main` moved first (THE-70 sidebar/doctor, THE-83 agent assets/harness,
bundled skills). Merged clean — no textual conflicts; the pre-commit treefmt
hook reformatted three incoming THE-70 pipeline docs and refused the
auto-commit, resolved by committing with the format applied (`ec82648d`).
The full-branch diff `git diff main...HEAD` was then reviewed.

## 2. Design conformance — every finding addressed

| #   | Design finding                         | Implementation                                                                                                                                                                                                                                | Verdict          |
| --- | -------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------- |
| F1  | 1-column separator grab                | `drag_hit::sep_grab` two-column band (`{sep, sep+1}` sidebar / `{sep-1, sep}` panel), wired at both grab arms; extra cell gated on `sep_is_exact(sep, mx) \|\| hit_pane.is_none()` — the drawer-at-`center_x` case is covered, exactly §3.1   | conformant       |
| F2  | press mutates (Wide drop-out on press) | grab state `(press_x, sep, moved, snapshot[, expanded])`; press arms only; Wide drop-out on the first moved sample                                                                                                                            | conformant       |
| F3  | Esc doesn't cancel separator drags     | Esc arm extended to both grabs, restoring the snapshot; **hardened in review** (below)                                                                                                                                                        | conformant + fix |
| F4  | pane-frame press swallowed the click   | `pane_lift` carries `(pane, press_x, press_y, moved)`; motionless release focuses the pane with the content-click path's exact two lines; moved release commits as before                                                                     | conformant       |
| F5  | seam ignores `pane_padding`            | `border_at(…, slop)`; call site passes `PANE_HPAD`; content early-return keeps any slop off app input; slop-0 behavior threaded through every existing test                                                                                   | conformant       |
| F6  | blank tail is a dead drop zone         | `row_at_clamped` (first/last on paint order, `None` on empty); `spot_at` clamps **inside the rect only**; press paths keep strict `row_at`; `spot_for_hover` validation untouched — converts "nothing" into "nearest row", never a wrong drop | conformant       |
| F7  | dead gap between tab chips             | `center_tab_hit` span `[sx, sx+w+1)` clamped at the shared `strip_chip_end` helper; placement math byte-identical; pre-existing `None` guards pass untouched                                                                                  | conformant       |

Chunk specs' exact API names/signatures (`SepSide`, `sep_grab`, `sep_is_exact`,
`sep_follow`, `row_at_clamped`), file ownership, and commit subjects all
honoured. Help prose (sidebar/panel/terminal-and-panes) describes shipped
behavior; the openspec change `fix-drag-grab-precision` is well-formed
(MODIFIED sidebar-width requirement keeps all existing scenarios incl. rail
refusal, ADDED row-drag drop-target + panel band requirements; pane-frame
behavior documented in the change's own design, no invented capability).

## 3. Invariants checked on the diff

- **Render decision pure** — all paths set `dirty`/`sidebar_dirty`/
  `need_relayout` only; no `selection_only`; drag feedback → Full. Esc's
  `dirty = true` covers the sidebar repaint (chrome ⇒ Full); `sidebar_dirty`
  redundancy in the motion arms is harmless.
- **0% idle** — every gesture step is an inbound mouse/key event; no timers,
  no wake sources; `drain_drag_events` untouched.
- **Ratchets** — no new ignored `Result`s, color/glyph literals, platform
  `cfg`, or `ACTION_SPECS` ids (grep-verified on the diff); help ratchet
  suite 71/71; new module `drag_hit` is substrate-free and
  `run.rs` carries loop-local tuples only.
- **Purity + tests** — all geometry is pure and unit tested at the module;
  no `thegn-core` change (coverage gate unaffected).

## 4. Verification run by the reviewer (on the review tree)

- `just quick thegn-host` — clean (clippy `-D warnings`), after merge and
  after the review fixes.
- `cargo nextest run -p thegn-host` (drag_hit, center_tab, pin_chip, row_at,
  sidebar_mouse, pane_drag, border_at, drop_on filters) — 54/54.
- Full crate unit suite (`tests::` filter) — **2385 passed, 50 skipped**.
- `openspec validate --all --strict` (pinned store binary) — **170/170**.
  (`just openspec-validate` fails outside `nix develop`: `command not found`
  — environmental, not a defect; same binary is pinned in `nix/openspec.nix`.)
- Merge addendum honoured; every "Unverified" section in chunk-1/2/3-done.md
  individually verified or accepted — resolution notes appended to each done
  artifact.

## 5. Review fixes applied (commit `a9829c82`)

Small corrections within the fix-or-flag mandate, both in `run.rs`:

1. **Esc half-applied the Wide expand.** The first moved sidebar sample calls
   `sb.collapse_wide()`, which persists `sidebar_expanded = "0"` — a DB
   mutation, not just in-memory state. Esc restored only `sb.width`, leaving
   the sidebar un-expanded in memory _and_ across restart. The grab now
   snapshots the pre-drag `expanded` flag (tuple's 5th field) and Esc
   restores it, writing the flag back. This is design §1 rule 4 ("restoring
   the pre-drag state … never half-applies") taken literally; the chunk spec
   had under-specified it as width-only.
2. **Stale "drag to resize" hint.** The old release path's width report was
   the documented prompt-clear for a motionless release (its comment said
   so); the new moved-only report let the press-time hint survive a bare
   click and an Esc cancel indefinitely. Both motionless release paths and
   the Esc cancel now `model.status.clear()` — a bare click is a no-op
   (rule 2), and a no-op leaves no drag prompt standing.

Post-fix: clippy clean, full crate unit suite green (2385), treefmt gate
green.

## 6. Accepted residuals (not revision-worthy)

- **Manual/e2e gesture feel** — unit-verified only, by the lead's no-e2e
  constraint. The pre-PR gate (tasks.md 4.3: `just ci` + deliberate
  `just e2e-update` if any recorded frame shifts) remains open by policy.
- **Separator drags are not in `pre_dispatch`'s pointer-capture flag**
  (only the sidebar row drag and the pane grabs are), so mid-drag motion over
  a _mouse-reporting_ pane app (vim, htop) is consumed by the app and the
  resize skips samples until the pointer leaves the pane. Pre-existing shape
  (the old 1-column drag had the same hole), not widened by this branch;
  candidate for a follow-up issue, not a THE-67 gap.
- **Motionless pane-border release still persists the session layout**
  (a no-op write). Pre-existing, untouched by this branch.

## 7. Verdict

**APPROVED** — design conformance, invariants, tests, and prose artifacts all
check out; the two review fixes are applied and green. Ready for the pre-PR
heavy gates (`just ci`; e2e re-record is the separate deliberate step noted
in the change's tasks.md 4.3).
