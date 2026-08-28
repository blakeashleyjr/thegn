# Chunk 2 done — Loop wiring: the separator grab, the pane gestures, docs + openspec

Issue THE-67 · branch `tg/the-67-drag-precision` · spec:
`.thegn/pipeline/THE-67/code/chunk-2.md`

## What landed

A previous coder left uncommitted, near-complete edits (run.rs, pane_drag.rs,
drag_hit.rs) before dying on a proxy outage. I audited them line-by-line
against the chunk spec and the architect design, completed the missing pieces,
and committed in three commits:

| Commit                                                                       | Content                                                                                                         |
| ---------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------- |
| `a6f108fa` `fix(the-67): forgiving separator + pane drag grabs, Esc cancels` | The chunk's exact subject: all loop wiring + seam slop + click-to-focus + the `drag_hit.rs` wiring-gate removal |
| `4602bf5b` `docs(the-67): help prose for the forgiving drag grabs`           | sidebar.md, panel.md, terminal-and-panes.md                                                                     |
| `d8458db0` `docs(the-67): openspec change for the drag grab-precision fix`   | `openspec/changes/fix-drag-grab-precision/` (proposal, design, tasks, delta specs)                              |

## Spec coverage (done criteria → code)

- **Two-column separator bands** — `run.rs` grab arms use
  `drag_hit::sep_grab` (band `{sep, sep+1}` sidebar / `{sep-1, sep}` panel)
  with the extra cell gated on
  `sep_is_exact(sep, mx) || hit_pane.is_none()` — the separator column always
  grabs; the furniture cell is skipped when it is pane/drawer content
  (non-full-width bottom drawer sits at `center_x`).
- **Press arms, motion commits** — grab state is
  `Option<(press_x, sep, moved, width_snapshot)>` per separator. Grab records
  the tuple + hint status only: **no `collapse_wide`, no persist, no width
  report** on the press (F2 gone). Motion at the press column does nothing;
  the first moved sample sets `moved` and (sidebar) does the Wide drop-out.
- **Grab-offset follow** — widths come from `drag_hit::sep_follow` through
  the **unchanged** clamp expressions (`clamp(30, (cols/2).max(30))` /
  `clamp(SIDEBAR_MIN_WIDTH, sidebar_max_width().max(SIDEBAR_MIN_WIDTH))`).
- **Release** — moved: the same `db_task::persist` calls + width report as
  before. Motionless: grab cleared, nothing persisted, no report.
- **Esc cancels (F3)** — the existing gesture-cancel arm (which already
  cleared `pane_lift`/`pane_border_grab`) now also cancels either separator
  grab: restores the snapshotted `sb.width` / `panel_cols_pref` (only when
  moved), re-applies the panel width via `layout::set_panel_width_cfg`,
  recomputes chrome, `need_relayout` + `dirty`, persists nothing.
- **Seam slop (F5)** — `border_at(frames, mx, my, slop)` widens the seam band
  `slop` columns past the two border cells on each side, both axes; call site
  passes `crate::center::PANE_HPAD` (Relaxed load). Content early-return
  untouched and prior — no slop value can steal a content cell (test at
  slop 3). Slop-0 behavior pinned by threading `0` through every existing
  test (none deleted).
- **Click-to-focus (F4)** — `pane_lift` is
  `Option<(PaneId, press_x, press_y, moved)>`; motion sets `moved` on leaving
  the press cell; a motionless release skips the swap/anchor and focuses the
  lifted pane with the content-click path's two lines
  (`focus.zone = Center`, `tab.focused_pane = id`), clearing the hint.
- **`drag_hit.rs` wiring gates off** — chunk 1's
  `#[cfg_attr(not(test), expect(dead_code))]` attributes removed; this change
  is the first non-test caller, so unfulfilled `expect`s would fail clippy.
- **Help pages** — sidebar.md row-drag bullet (nearest-row drop + outside
  cancels, chunk 3) and width section (divider **or** pane edge, motionless
  click changes nothing, Esc cancels); panel.md width drag (same semantics);
  terminal-and-panes.md "Mouse on a pane's frame" bullet (frame click
  focuses, drag rearranges, Esc abandons). No new `ACTION_SPECS` ids — help
  ratchets untouched.
- **OpenSpec change** — `openspec/changes/fix-drag-grab-precision/` modeled on
  `add-pipeline-board`: proposal cites THE-67 + tasks.md groups **B**
  (items 22/25) and **G** (item 98); design.md owns the pane-frame behavior
  (F4/F5 — no capability spec); delta specs: sidebar **MODIFIED**
  "Configurable, resizable sidebar width" (keeps all three existing scenarios
  incl. "Rail refuses a resize", adds 5) + **ADDED** row-drag drop-target
  requirement; panel **ADDED** separator-grab-band requirement. Not synced /
  not archived, per spec.

## Invariants

Render decision pure (`dirty`/`need_relayout`/`sidebar_dirty` only; drag
feedback → `RenderPlan::Full`), nothing sets `selection_only`, no new wake
sources/timers (all steps are inbound-mouse-driven), no new ignored `Result`s,
no color/glyph literals, no platform `cfg`, `run.rs` carries loop-local tuple
state only (all geometry lives in the pure, unit-tested modules).

## Verification (all on the final committed tree)

- `just quick thegn-host` — clean (6m33s, one-shot compile after the treefmt
  reformat).
- `cargo nextest run -p thegn-host pane_drag` — 9 passed (incl. the three new
  slop tests: pad-cell seam hit at slop 1 / miss at slop 0, horizontal seam at
  slop 2, content never stolen at slop 3).
- `cargo nextest run -p thegn-host border_at` — 6 passed;
  `... drop_on` — 4 passed; `... drag_hit` — 7 passed.
- `cargo nextest run -p thegn-host 'tests::'` — **2353 passed, 50 skipped**,
  0 failed (the filter happens to match every unit-test module, i.e. the
  crate's full unit suite ran green on the final tree).
- OpenSpec: `openspec validate --all --strict` — **169 passed, 0 failed**.

## Unverified

- **`just openspec-validate` via the `just` recipe** — failed with
  `openspec: command not found` because this shell is not inside
  `nix develop`. I ran the identical command with the pinned store build
  (`/nix/store/9z1…-openspec-1.6.0/bin/openspec validate --all --strict`,
  the same binary `nix/openspec.nix` pins): 169/169 green. The recipe itself
  should pass in the dev shell.
- **`just test`, `just ci`, `just coverage`, `just e2e`** — deliberately not
  run (dev-loop policy; the chunk spec forbids them here). The full-workspace
  gates + e2e re-record (`just e2e-update` — frames changed, baselines stale
  by construction) are the pre-PR / follow-up step, noted as open item 4.3 in
  the change's tasks.md.
- **Manual mouse interaction** — the loop wiring is verified by compile +
  the crate's unit suite only; the gesture feel (threshold, follow offset,
  Esc restore) was not exercised in a live TUI session (no e2e per the lead's
  constraints).

## Architect review verification (post-merge, commit a9829c82)

- **openspec**: re-ran the pinned binary on the review tree — 170/170 strict.
  The `just` recipe failure is environmental (outside `nix develop`);
  `nix/openspec.nix` pins the same binary. Not a defect.
- **Heavy gates + e2e re-record**: remain the pre-PR gate (tasks.md 4.3),
  consistent with the dev-loop policy and the chunk spec's exclusion.
- **Manual mouse feel**: reviewed the full loop diff against design §3.1/§3.2
  line-by-line; two defects found and fixed in commit a9829c82 (Esc Wide
  half-apply; stale drag hint on motionless release / Esc) — see
  `architect-review/verdict.md`.
