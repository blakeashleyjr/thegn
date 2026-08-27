# THE-75 chunk 3 — done

Branch `tg/the-75-monitor-fixes`. Covers audit items **10 (M)** and **8 (M)**
plus the two S-effort docs findings (design §1.4, §1.5, §1.9, §D6–§D7).

Commits (oldest first):

| sha        | subject                                                                                          |
| ---------- | ------------------------------------------------------------------------------------------------ |
| `c22c7c57` | `wip(monitor): PipelineLanding, footer.rs and a Help outcome (THE-75)`                           |
| `71f4581a` | `feat(monitor): Enter opens a board row's worktree, plus per-tab hints and a help door (THE-75)` |

The final commit subject is the exact string the chunk spec required.

## What landed

### 1. `Enter` on a board row opens the worktree

**`monitor_action.rs`** — `pipeline_target` is gone; `pipeline_landing(&jump,
&model) -> PipelineLanding` replaces it, pure over the model:

- `Row(RowTarget)` — the sidebar-row match, **byte-for-byte the predicate that
  was there before** (`RowKind::Worktree` + `worktree_path` + `tab_target
.is_some()`), tried first, so the working case cannot have changed.
- `Open { tab_name, path }` — `model.sidebar_db_worktrees` looked up by `path`.
  This is the agent-supervision case the design named: `gather_groups` only
  synthesises DB rows for a _dormant_ workspace, so a worktree of the current
  workspace with no resident group had no row and dead-ended.
- `None` — neither source knows it.

The doc note that `PipelineJump::session` is carried but unused (pane focus is
phase 2) is kept.

**`run.rs`** — the `MonitorAction::Pipeline` arm is now a three-way match.
`Row` is the previous body verbatim. `Open` mirrors
`handlers::creating::open_tab`: `session.worktrees.iter().position(|g| g.name ==
tab_name)` ⇒ `switch_to_tab(gi, 0)`, else
`add_group(WorktreeGroup::new(tab_name, GroupKind::Branch, path))` — so a second
`Enter` switches rather than duplicating — then the same tail as `Row`
(`monitor = None`, `focus.zone = Center`, `refresh_tab_model`, `need_relayout`,
`dirty`, `continue`). A comment records why `RowTarget::Workspace` was rejected
(`switch_workspace` early-returns for the current workspace and its `land_on` is
a silent no-op for a non-resident group). `None` keeps the existing notice, its
comment corrected to "deleted under the board, or never registered".

### 2. Footer hints, gated per tab

**`monitor/footer.rs` (new)** — `MonitorOverlay::footer`'s whole body lifted out
behind `pub(super) struct FooterInput<'a>` / `pub(super) fn line(...) -> Line`,
so the builder needs no overlay. `monitor.rs`'s `footer()` is now a 12-line
delegation that fills the struct. `mod footer;` sits beside `mod build;`.

- New `MonitorTab::has_graphs()` — `false` for Procs/Containers/Pipeline. Its
  doc says it is what the footer gates `[ ]`/`g`/`s` on, and that the toggles
  still _work_ on those tabs, they just move nothing on screen.
- `[ ]` window / `g` style / `s` scale appear **only** when `has_graphs()`.
- `spc pause/resume` appears on **every** tab, Pipeline and Containers included.
- `tab tabs` stays first; `? help` is appended last on the left; the right slot
  is the transient status or `q close`, as before.
- The confirm / filter / notice arms are content-identical to the originals
  (same precedence, same strings, same `\u{2502}` escape — no glyph literal).

Internally the hints are built as a `Vec<Vec<Seg>>` joined by a `"  "`
separator, replacing the hand-carried trailing spaces. Rendered text is
unchanged for the arms that survived.

### 3. The help door

- `MonitorOutcome::Help` (enum stays `Copy`), documented with the reason it is
  not a `Passthrough`: `help::open` resolves from focus zone / panel section and
  the monitor is neither, so a handed-back key would open help for whatever is
  focused _behind_ the modal.
- `handle_key`: `KeyCode::Char('?') | KeyCode::Function(1) => Help`, placed with
  the global keys immediately after the `q` arm — above the per-tab letter arm
  (so Processes cannot shadow it) and below the filter/confirm early-returns (so
  a typed `?` still lands in a filter query). F1 previously matched no arm at all
  and fell to `Pending`, i.e. the modal ate the global help key.
- `run.rs`: a `Help` branch in the same `if/else` chain as `Close`/`passthrough`,
  calling `help::open_at(&help_registry, keymap.config(),
crate::help::context::MONITOR)` and leaving the monitor open. Verified by
  reading the loop that this stacks correctly: the help overlay renders _after_
  the monitor (`run.rs:12035`) and owns every key while open (`run.rs:13489`,
  checked before the monitor block), so closing help returns keys to the monitor.
- `help/context.rs`: `pub const MONITOR: &str = "overlay:monitor"` pushed into
  `vocabulary()`, plus a module-doc paragraph explaining that the vocabulary now
  covers full-screen modals that own the keyboard — keys `resolve` will never
  return, because `resolve` only ever sees the focus state.

### 4. `docs/help/system-monitor.md`

- Frontmatter: `parent: bars` dropped, `order: 8`, `contexts: [overlay:monitor]`;
  `actions:` unchanged. Ties break by title, so it lands right after "Command
  palette" (also `order: 8`).
- Tabs: every tab is numbered, `0` reaches the tenth, and the strip scrolls to
  keep the active tab whole and marks what scrolled off each end.
- Ladder: rewritten as "the rungs of `[monitor] window_ladder`, starting at
  `[monitor] default_window`", with a note that it is configurable — no
  hard-coded list to drift again. The stale `30s, 2m, 10m, 1h, all` is gone.
- New paragraph on the list tabs' row cursor (arrows/`j`/`k` move the cursor and
  the view follows), repeated beside the `x` paragraphs on Processes and Disk.
- New paragraph on `?` / `F1` and on the footer advertising only the current
  tab's keys.
- Pipeline: the "`1`–`9` stop short of it" apology deleted (`Alt-b` stays as the
  direct door); a paragraph on configured-but-idle stages and the
  agent/concurrency/next on each heading; `Space` freezing the board _and_ its
  re-read; and `↵` now opening a non-resident worktree, the notice surviving only
  for one that is genuinely gone. The "the board is a **view**, not a
  controller" paragraph is untouched and unqualified.

## Tests

All nine from the spec.

`monitor_action.rs::pipeline_tests` (the two old tests replaced):

1. `a_resident_worktree_still_resolves_to_its_sidebar_row`
2. `a_registered_but_unopened_worktree_resolves_to_open` (with a targetless
   decoy row for the same path, so the old predicate's negative case is kept)
3. `an_unknown_worktree_resolves_to_none` (wrong-kind row + a non-matching DB row)
4. `a_sidebar_row_wins_over_the_db_row`

`monitor_tests.rs`:

5. `the_footer_only_advertises_keys_the_tab_has`
6. `every_tab_advertises_pause` (all ten, plus `resume` on a frozen Pipeline)
7. `the_footer_advertises_help_on_every_tab` (also asserts it precedes `q close`)
8. `question_mark_and_f1_ask_for_help` (CPU **and** Procs, plus `?` typed while
   filtering ⇒ `Pending` and `ov.filter == "?"`)
9. `has_graphs_matches_what_the_builders_emit`

New test helpers: `footer_for(tab)` and `graph_hints(tab)`, both driving the
pure `footer::line` builder rather than an overlay. Test 9 is the strong form:
for every `MonitorTab::ALL` it opens the real overlay on a fixture where every
tab is present and asserts `has_graphs() == ov.body` contains a
`Section::Graph`, so a new tab that emits plots but forgets the gate fails.

### What was run (scoped only, per the dev-loop policy)

```
just quick thegn-host                                  clean (clippy -D warnings)
cargo nextest run -p thegn-host monitor --no-fail-fast  101/101 pass
cargo nextest run -p thegn-host help   --no-fail-fast    73/73 pass
cargo nextest run -p thegn-host ratchet --no-fail-fast   12/12 pass
```

The `help` run covers `registry_validates_cleanly`,
`claimed_actions_are_mentioned_in_the_page_body`,
`every_panel_context_has_a_documentation_page`, `every_zone_has_a_documentation_page`,
`action_docs_ratchet` and `full_shipped_pages_render_at_common_widths` — all
green, and it was re-run **after** treefmt reformatted the markdown.

`git status test/` is empty: `test/help-ratchet.txt`,
`test/help-prose-ratchet.txt` and `test/help-context-ratchet.txt` are
**unmodified**, and so are `test/glyph-literal-ratchet.txt` /
`test/color-literal-ratchet.txt` — `footer.rs` needed no entry (`↵ ↓ ↑ ·` are
outside the U+2500–U+259F box-drawing range the ratchet scans, and the `│` in
the filter echo stayed a `\u{2502}` escape). No new `ACTION_SPECS` entry, no
keymap edit, no new `let _ =` / `.ok()`.

## Deviations from the spec (deliberate, both minor)

1. **`run.rs` uses `crate::help::context::MONITOR` rather than the literal
   `"overlay:monitor"`.** The spec writes the literal at both sites; a `pub
const` beside `vocabulary()` means the call site and the vocabulary entry
   cannot drift into a silent index-page fallback. The string is identical.
2. **`FooterInput` also carries `filtering`/`filter`/`confirm`/`notice`/`status`**
   — the spec's list named "confirm/filter/notice/status", so this is just the
   filter split into its two fields (the mode flag and the query text).

## Notes for review

- The footer's Containers/Pipeline arms moved from early `return`s into the
  shared hint list, so those two tabs now get `spc pause` and `? help` _and_ the
  `tab tabs` prefix in one construction rather than three. The rendered strings
  for the surviving hints are unchanged.
- `Disk`'s `x clean` hint is still gated on `disk_rows > 0`, as before.
- Nothing in `sections.rs`, `thegn-core`, `monitor_pipeline.rs`,
  `monitor/build.rs` or `monitor/tabbar.rs` was touched.

## Unverified

- **No full-workspace gate was run** (`just test`, `just ci`, `just coverage`,
  `just lint`), per the chunk spec and the Lead addenda. Tests outside the
  `monitor` / `help` / `ratchet` filters were not executed; the whole-crate
  `cargo nextest run -p thegn-host` is the Lead's pre-push gate. `thegn-core`
  was not touched, so its 95% coverage gate should be unaffected — not measured.
- **`just e2e` was not run.** As chunks 1 and 2 recorded, every chunk in this
  lane changes drawn frames — this one changes the footer on **every** tab — so
  all 45 baselines under `test/muse/snapshots/` are stale for this branch.
  Re-recording with `just e2e-update` belongs to whoever revives that gate.
- **The `Open` branch was not exercised end-to-end**, only its pure resolution
  (tests 2–4). Driving it needs a live `Session` + `Panes` inside the event loop;
  the branch itself is a transcription of `handlers::creating::open_tab`'s two
  lines, and it typechecks, but "the pane actually materialises when you press
  `↵` on a non-resident board row" was reasoned from
  `handlers/materialize.rs:51-61` (a `CenterTree::Leaf(0)` missing leaf is picked
  up lazily), not observed.
- **The new help page's placement was not eyeballed in the running app** — only
  that the registry validates and every shipped page still renders at the common
  widths. Order-8-ties-break-by-title comes from
  `thegn-core/src/help/registry.rs`, read rather than run.
- **The `?` key on a terminal that reports it with SHIFT held** takes the same
  arm (only ALT/SUPER/CTRL early-return before the match), reasoned from the
  handler's structure; the tests press it with `Modifiers::NONE`.
- The Containers help section still describes the old header wording ("how many
  owned containers, images and volumes") rather than chunk 2's
  `{owned} owned · {foreign} foreign`. Out of this chunk's file scope and not in
  its spec, so it was left alone — worth a one-line docs follow-up.
