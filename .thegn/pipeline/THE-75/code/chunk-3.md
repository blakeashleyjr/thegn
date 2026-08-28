# THE-75 chunk 3 — Enter opens the worktree; per-tab footer hints, a help door, and the help page

Read `.thegn/pipeline/THE-75/architect/design.md` §1.4, §1.5, §1.9 and §D6–§D7
first — the evidence and the rationale are there; this file is the work order.

Covers audit items **10 (M)** and **8 (M)**, plus the two S-effort docs findings
(help page filed under `bars`; the `[`/`]` ladder documented wrong).

## Ordering / overlap

- **Runs THIRD. Serial.** Shares `monitor.rs`, `monitor_action.rs`, `run.rs` and
  `monitor_tests.rs` with chunks 1 and 2. Do not start before chunk 2 has landed.

## Files touched (exact)

| Path | Why |
|---|---|
| `crates/thegn-host/src/monitor_action.rs` | `PipelineLanding`, `pipeline_landing()` replacing `pipeline_target()` |
| `crates/thegn-host/src/run.rs` | The `MonitorAction::Pipeline` arm's new `Open` branch; the `MonitorOutcome::Help` arm |
| `crates/thegn-host/src/monitor.rs` | `MonitorOutcome::Help`, the `?`/F1 key arm, `footer()` delegating to the new module, `mod footer;` |
| `crates/thegn-host/src/monitor/footer.rs` | **NEW** — the footer `Line` builder, lifted out with per-tab gating |
| `crates/thegn-host/src/help/context.rs` | `"overlay:monitor"` added to `vocabulary()` |
| `docs/help/system-monitor.md` | Top-level page, context claim, corrected ladder, new keys |
| `crates/thegn-host/src/monitor_tests.rs` | New tests |

Do **not** touch: `sections.rs`, `thegn-core`, `monitor_pipeline.rs`,
`monitor/build.rs`, `monitor/tabbar.rs`. Do not add a keymap action or edit
`keymap_specs.rs`.

## Work

### 1. `Enter` on a board row opens the worktree (`monitor_action.rs`, `run.rs`)

`pipeline_target` (`monitor_action.rs:272-285`) only ever resolves worktrees
that already have a sidebar row with a `tab_target`. Rows are synthesised from
the DB **only for a dormant workspace** (`sidebar.rs:1210`'s `if !live` guard),
so a worktree of the *current* workspace that has no resident group has no row
— exactly the agent-supervision case — and the loop falls to the
`"no open worktree for …"` notice (`run.rs:13676-13690`).

Replace it with a pure three-way resolution:

```rust
/// Where an `Enter` on a board row should land.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PipelineLanding {
    /// A sidebar row already targets it — the existing door, unchanged, so the
    /// board can never drift from sidebar navigation.
    Row(crate::sidebar::RowTarget),
    /// Registered in the DB but not resident in this session (a dispatch made
    /// by another process onto a worktree this session never opened). Open it
    /// as a group: its tab is a `CenterTree::Leaf(0)` missing leaf, so the
    /// lazy materialize path spawns the pane — the same route a freshly
    /// created worktree takes.
    Open { tab_name: String, path: String },
    /// Nothing known — deleted under the board, or never registered.
    None,
}

/// Pure over the model, so both arms are unit-testable.
pub fn pipeline_landing(jump: &PipelineJump, model: &FrameModel) -> PipelineLanding
```

- Try the existing sidebar-row match **first**, unchanged (`RowKind::Worktree`
  + `worktree_path` + `tab_target.is_some()`). Nothing about the working case
  may change.
- Otherwise look `jump.worktree` up in `model.sidebar_db_worktrees`
  (`chrome.rs:458`; `DbWorktree` at `sidebar.rs:476-492`) by `path`, and return
  `Open { tab_name: w.tab_name.clone(), path: w.path.clone() }`.
- Otherwise `None`.
- Keep the doc note that `PipelineJump::session` is carried but unused
  (pane-level focus is phase 2).
- Delete `pipeline_target`; update its two existing tests
  (`monitor_action.rs:287-334`) to the new enum and add the `Open` and `None`
  cases.

In `run.rs`'s `Some(MonitorAction::Pipeline(jump))` arm (`run.rs:13641-13691`):

- `Row(target)` — the existing body verbatim.
- `Open { tab_name, path }` — mirror `handlers::creating::open_tab`
  (`handlers/creating.rs:81-95`):
  - if `session.worktrees.iter().position(|g| g.name == tab_name)` is `Some(gi)`,
    just `session.switch_to_tab(gi, 0)` (idempotent — never a duplicate group);
  - else `session.add_group(WorktreeGroup::new(tab_name, GroupKind::Branch, path))`;
  - then close the monitor (`monitor = None`), `focus.zone = Zone::Center`,
    `refresh_tab_model(&mut model, &session, &mut sb)`,
    `need_relayout = true; dirty = true; continue;` — the same tail the `Row`
    branch already uses, and for the same reason (the jump changes what the
    center band shows, so leaving the modal over it would hide the thing asked
    for).
  - **Do not** route this through `RowTarget::Workspace`: `switch_workspace`
    returns early when the target IS the current workspace and its `land_on` is
    a silent no-op for a non-resident group (`run.rs:1976-1987`) — the same dead
    end in a different costume. Say so in a comment.
- `None` — the existing notice, unchanged.

### 2. Footer hints, gated per tab (`monitor/footer.rs`, `monitor.rs`)

Lift `MonitorOverlay::footer` (`monitor.rs:1374-1506`) into a new
`crates/thegn-host/src/monitor/footer.rs` (`mod footer;` beside `mod build;`).
`monitor.rs` is on the "don't grow the god-files" list; new chrome goes in a
sibling. Take an explicit input struct rather than `&MonitorOverlay` so the
builder is testable without an overlay:

```rust
pub(super) struct FooterInput<'a> { … }   // tab, prefs, confirm/filter/notice/status,
                                          // paused, proc toggles, the selected
                                          // container's `ours`, disk row count
pub(super) fn line(input: FooterInput<'_>) -> Line
```

Behaviour changes (the confirm / filter / notice arms keep their current
precedence and content verbatim):

- Add `MonitorTab::has_graphs(self) -> bool` on the enum in `monitor.rs` —
  `true` for Cpu/Memory/Thermal/Network/Disk/Gpu/Power, `false` for
  Procs/Containers/Pipeline. Document that it is what the footer gates the
  `[ ]` / `g` / `s` hints on: Processes emits only headings and a table
  (`build.rs:777-875`), so advertising four graph keys it has no graph for is
  the bug.
- Show `[ ]` window, `g` style and `s` scale **only** when `has_graphs()`.
- Show `spc pause/resume` on **every** tab, Pipeline and Containers included —
  `Space` freezes the board (`monitor.rs:937-948`) and stops its roster sample
  (`wants_dispatches`, `:555-557`), and the board's footer hiding that is how a
  supervisor ends up staring at a stale board.
- Keep `tab tabs` first and `q close` (or the transient status) on the right, as
  today.
- Append a `? help` hint on every tab, immediately before the right-hand slot.

### 3. The help door (`monitor.rs`, `run.rs`, `help/context.rs`)

- New `MonitorOutcome::Help` variant (keep the enum `Copy`), documented as: the
  monitor is neither a focus zone nor a panel section, so `help::open`'s
  context resolution (`help/context.rs:26-31`) would land on whatever is
  focused *behind* the modal — hence a dedicated outcome, not a `Passthrough`.
- `handle_key`: `KeyCode::Char('?')` and `KeyCode::Function(1)` return
  `MonitorOutcome::Help`. Place the arm with the other global keys, **above**
  the per-tab arm (`monitor.rs:1016-1020`) so the Processes tab's letters
  cannot shadow it, and **below** the filter/confirm early-returns
  (`:861-866`) so a typed `?` still lands in a filter query.
- `run.rs`: where the outcome is inspected (`run.rs:13594-13601`), handle
  `Help` by `help_overlay = crate::help::open_at(&help_reg, &current_config,
  "overlay:monitor")` and leave the monitor open — the help overlay already
  renders after it (`run.rs:12018-12036`), so it stacks correctly. Use whatever
  local already holds the registry at the other `help::open` sites
  (`run.rs:12994`, `:18769`).
- `help/context.rs`: add `"overlay:monitor"` to `vocabulary()` (`:33-58`).
  `contexts:` entries are validated against that list, so the page's claim would
  otherwise be a registry error. Extend the module doc: the vocabulary now
  covers full-screen modals that own the keyboard, not just zones and panel
  sections.

### 4. `docs/help/system-monitor.md`

- Frontmatter: **drop `parent: bars`**, set `order: 8`, add
  `contexts: [overlay:monitor]`. Keep `actions: [open-monitor, open-pipeline-board]`.
  The monitor is a full-screen modal reached by a global chord and the palette,
  not a bar feature; every other full-surface page (`sidebar`, `panel`,
  `terminal-and-panes`, `search`) is top level. Order ties break by title
  (`thegn-core/src/help/registry.rs:247`), so sharing `8` with the command
  palette is fine and lands the monitor right after it.
- **Tabs section (`:24-26`)**: the bar now numbers each tab, `0` reaches the
  tenth, and the strip scrolls to keep the active tab whole on a narrow
  terminal.
- **Pipeline section (`:155-156`)**: delete the "the `1`–`9` tab digits stop
  short of it" apology — `0` reaches it now. `Alt-b` stays the direct door.
- **Ladder (`:44`)**: the rungs are `30s, 1m, 5m, 10m, 30m, 1h, 6h, 12h, all`
  and the default is `1m`
  (`thegn-core/src/series_window.rs:144-145`, `config.rs:2697-2700`). `2m` has
  not been a rung for some time — a test pins that (`monitor/state.rs:314-325`).
  Say the ladder is `[monitor] window_ladder` and configurable, rather than
  restating a hard-coded list that can drift again.
- **New keys**: `?` (or `F1`) opens this page from inside the monitor; the
  footer advertises only the keys the current tab actually has.
- **List tabs**: on Processes, Disk, Containers and Pipeline the arrows/`j`/`k`
  move a **row cursor** (the highlighted row) and the view follows it — so `x`
  always acts on the row you can see. Say this in the Processes and Disk
  sections beside their `x` paragraphs; it is the user-facing half of the safety
  fix.
- **Pipeline section**: `Space` freezes the board (and pauses its roster
  re-read); `↵` on a row now **opens** the dispatch's worktree when it is not
  already a tab, instead of reporting that it isn't open — the notice remains
  only for a worktree that is genuinely gone.
- Board display: configured stages appear even when idle, with their agent,
  concurrency and `next` beside the live count. Keep the existing
  "the board is a **view**, not a controller" paragraph (`:179-184`) intact and
  unqualified — none of this makes thegn advance a stage.
- Keep every claimed action id genuinely described: the help prose ratchet
  requires the page to mention what it claims, not merely list it.

## Tests

`crates/thegn-host/src/monitor_action.rs` (extend the existing
`pipeline_tests` module):

1. `a_resident_worktree_still_resolves_to_its_sidebar_row` — the unchanged case.
2. `a_registered_but_unopened_worktree_resolves_to_open` — no sidebar row, one
   matching `sidebar_db_worktrees` entry ⇒ `Open { tab_name, path }`.
3. `an_unknown_worktree_resolves_to_none` — neither source knows it.
4. `a_sidebar_row_wins_over_the_db_row` — both present ⇒ `Row`, so the existing
   door keeps precedence.

`crates/thegn-host/src/monitor_tests.rs`:

5. `the_footer_only_advertises_keys_the_tab_has` — Processes' footer contains no
   `[ ]` / `g` / `s` hint; CPU's contains all three.
6. `every_tab_advertises_pause` — including Pipeline and Containers.
7. `the_footer_advertises_help_on_every_tab`.
8. `question_mark_and_f1_ask_for_help` — both return `MonitorOutcome::Help` on a
   graph tab and on Processes (the per-tab letter arm must not shadow them),
   and `?` typed **while filtering** lands in the filter query instead.
9. `has_graphs_matches_what_the_builders_emit` — a table-driven assertion over
   `MonitorTab::ALL` so a new tab cannot silently inherit the wrong footer.

Run, scoped only:

```sh
just quick thegn-host
cargo nextest run -p thegn-host monitor
cargo nextest run -p thegn-host help
```

Do **not** run `just test`, `just ci`, `just coverage`, or `just e2e`.

## Done criteria

- All nine tests pass; the three help ratchets still pass
  (`cargo nextest run -p thegn-host help` covers `ratchet_tests`), and
  `test/help-ratchet.txt`, `test/help-prose-ratchet.txt` and
  `test/help-context-ratchet.txt` are **unmodified** — this change claims a new
  context in the same commit that adds it to the vocabulary, so none of them may
  grow.
- The help registry validates cleanly (`registry_validates_cleanly`) with the
  page at top level and the new `contexts:` claim.
- `just quick thegn-host` is clean.
- `monitor.rs` did not gain a new subsystem — `footer.rs` is its own file.
- No new `ACTION_SPECS` entry, no keymap edit.
- Commit with **exactly** this subject:

```
feat(monitor): Enter opens a board row's worktree, plus per-tab hints and a help door (THE-75)
```

## Note for the reviewer / Lead

Every chunk in this lane changes drawn frames, so all 45 baselines under
`test/muse/snapshots/` are stale. e2e was **not** run (known-broken, opt-in
gate; the baselines were already stale as of `0f9c5a9a` per CLAUDE.md).
Re-recording with `just e2e-update` belongs to whoever revives that gate.
