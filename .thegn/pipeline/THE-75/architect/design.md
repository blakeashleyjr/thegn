# THE-75 — System monitor usability fixes: architect design

Branch `tg/the-75-monitor-fixes`. Linear: <https://linear.app/blakeashley/issue/THE-75>

Scope is the S/M remainder of the live MonitorTab audit. Every claim below is
cited at `file:line` against **this worktree's `main`-folded tree**
(`982ab7cb`), not against the audit notes.

---

## 0. Verification of the four "in flight on tg/pipeline-board-access" items

The issue asked these be re-checked on main first. All four are settled:

| # | Claim | Verdict on main | Evidence |
|---|---|---|---|
| 1 | `parse_chord` Shift synthesis | **Not ours** — owned by the THE-70 lane. Do not touch keymap chord parsing in this change. | (per issue) |
| 2 | `dispatched_at_ms` stored in SECONDS | **FIXED.** The insert writes `util::now_ms()` with a comment naming the old bug. A regression test drives the real clock end to end. `stage_blocked` divides by 1000 exactly once, into the seconds shape `AttentionInputs::stage_blocked_since` wants — correct, not a double divide. | `crates/thegn-core/src/db_notification.rs:305-312`; `crates/thegn-host/src/monitor_pipeline.rs:305-318`, `:401-416`, `:652-664` |
| 3 | `monitor_prefs.last_tab` dead code | **FIXED.** `remember_tab()` exists and is called from `switch()`, the digit keys and `goto_tab()`. | `crates/thegn-host/src/monitor.rs:752-760`, `:688`, `:749`, `:928` |
| 4 | Toggle doesn't toggle / `Ctrl-g` closes the monitor | **FIXED.** `MonitorOutcome::Passthrough` exists; `ALT|SUPER` chords are handed back **before** the CTRL arm (so the `Ctrl-Alt-Shift-M` open chord toggles shut), and `Ctrl-g` returns `Passthrough`. | `crates/thegn-host/src/monitor.rs:325-332`, `:876-890` |

Nothing to do for #1–#4. The rest of this document is the S/M lane.

---

## 1. What is actually wrong (evidence)

### 1.1 Tab bar (item 5, M)

`tab_bar()` renders bare labels with no digits, and lays them out as
`Line::split(left, right)` — `crates/thegn-host/src/monitor.rs:1320-1341`.

- **No digits.** The help page tells the user `1`–`9` jump to a tab
  (`docs/help/system-monitor.md:25-26`) but the bar never says which digit is
  which. The digit keys index the **visible** list (`monitor.rs:923-932`), so
  the mapping is machine-dependent and cannot be memorised.
- **No 10th key.** `MonitorTab::ALL` has ten entries (`monitor.rs:143-154`) and
  the digit arm only accepts `'1'..='9'`, so on a machine that shows every
  family the Pipeline board is unreachable by digit. The help page even
  apologises for it: *"the `1`–`9` tab digits stop short of it"*
  (`docs/help/system-monitor.md:155-156`).
- **Active tab gets clipped.** `draw_line`'s `Line::Split` arm cuts the LEFT run
  to `w - right_width - 1` (`crates/thegn-host/src/seg.rs:541-548`). Ten labels
  plus separators is ~78 cells; the box interior is `screen.cols * 4/5` clamped
  to ≥56 (`monitor.rs:501-511`). On an 80-column terminal the interior is 64
  cells, the coverage note eats ~10 more, and the last two tabs — including
  **Pipeline, where the cursor may be** — are silently cut off.

### 1.2 Row cursor (item 6, M)

Selection is one tinted cell. Four sites, all the same shape:

- Disk worktrees: `monitor/build.rs:583-587` — `name_tone = Accent` on the name cell only.
- Processes: `monitor/build.rs:831-836`.
- Containers: `monitor/build.rs:916-923` — **and here it is worse**: the `if cur`
  arm *replaces* the ownership tint, so selecting a row destroys the
  `Hue::Green` (ours) / `S::Ghost` (foreign) signal the whole tab is built on.
- Pipeline: `monitor/build.rs:1034-1039`.

`TableSection` has no notion of a selected row at all
(`crates/thegn-host/src/sections.rs:184-188`), so `draw_table`
(`sections.rs:557-586`) cannot paint one. `Seg` *does* carry
`bg: Option<Tok>` (`seg.rs:87-101`, `.bg()` at `seg.rs:153`) and `put_line`
takes a pad token, so a full-width row tint is expressible today — nothing is
using it.

The house vocabulary for a cursor row already exists and should be reused
verbatim: background `Tok::Slot(S::Panel2)` and the `half_block_r` (`▐`) bar
(`crates/thegn-host/src/sidebar_view.rs:1063`, `:258`).

### 1.3 Viewport does not follow the cursor — a safety bug (item 7, M)

`nav()` moves the row cursor **and** raw-scrolls by the same delta
(`monitor.rs:1027-1040`):

```rust
self.sel = (self.sel as isize + delta).clamp(0, max.max(0)) as usize;
self.scroll_by(delta);          // <-- independent of where `sel` actually is
```

The two clamp against **different** bounds — `sel` against `row_len()`
(`monitor.rs:1045-1053`), `scroll` against `stack_height - body_rows`
(`monitor.rs:658-660`) — so they diverge as soon as either saturates, and the
Disk/Processes tables sit *below* a graph, a volumes table and a grid, so they
start out of phase. `PageUp`/`PageDown` move `sel` by a whole page
(`monitor.rs:980-987`) while `End`/`G` move the viewport and leave `sel` where
it was (`monitor.rs:993-996`).

Consequence, and the reason this is a **safety** item rather than a polish one:
`x` on Processes signals `self.proc_rows[self.sel]` (`monitor.rs:1130-1155`) and
`x` on Disk cleans `self.disk_rows[self.sel]` (`monitor.rs:1175-1186`). Scrolling
therefore retargets a destructive key while the user is looking somewhere else,
and (per §1.2) the target is not visibly marked. The confirmation names the
target — which is the only thing standing between this and a wrong kill — but a
prompt that disagrees with what the eye is on is a trap, not a guard.

### 1.4 Footer hints (item 8, M)

`footer()` — `monitor.rs:1374-1506`:

- The generic arm (`:1454-1469`) advertises `[ ]` window, `g` style, `s` scale
  and `spc` pause on **every** non-Containers, non-Pipeline tab. Processes has
  no graph at all (`build.rs:777-875` emits only headings and a table), so four
  of its five advertised keys do nothing visible.
- The Pipeline arm (`:1422-1430`) shows only `tab` and `↵`. `Space` still
  **freezes the board** (`monitor.rs:937-948`, and `wants_dispatches()` at
  `:555-557` stops the roster sample while paused) with nothing on screen saying
  so — a supervisor who taps Space wonders why the board went stale.
- No help affordance anywhere. F1 is swallowed: it matches no arm and falls to
  `_ => MonitorOutcome::Pending` (`monitor.rs:1021`), so the modal eats the
  global help key.

### 1.5 `Enter` on a board row dead-ends (item 10, M)

`pipeline_target()` resolves a jump **only** against rows already in
`model.sidebar_rows` that carry a `tab_target`
(`crates/thegn-host/src/monitor_action.rs:272-285`). `gather_groups()` only
synthesises sidebar rows from the DB for a **dormant** workspace — the
`if !live` guard at `crates/thegn-host/src/sidebar.rs:1210` — so a worktree of
the *current* workspace that has no resident group has no sidebar row, hence no
target, and the loop falls to the notice arm
(`crates/thegn-host/src/run.rs:13676-13690`): *"no open worktree for …"*. That
is precisely the agent-supervision case: a dispatch created by another process
lands on a worktree this session never opened.

The materialisation door already exists and is one call —
`session.add_group(WorktreeGroup::new(tab_name, GroupKind::Branch, path))`
(`crates/thegn-host/src/handlers/creating.rs:82`); the group's tab is
`CenterTree::Leaf(0)` (`crates/thegn-host/src/session.rs:106-117`), i.e. a
missing leaf, which the lazy materialize path picks up
(`crates/thegn-host/src/handlers/materialize.rs:51-61`). And
`model.sidebar_db_worktrees: Vec<DbWorktree>` (`chrome.rs:458`) already carries
`repo_path` / `tab_name` / `path` for every registered worktree
(`sidebar.rs:476-492`).

Note also that `switch_workspace` short-circuits when the target IS the current
workspace and its `land_on` silently does nothing if the group is not resident
(`run.rs:1976-1987`) — so routing this case through `RowTarget::Workspace`
would reproduce the same silent dead end. It must be a distinct outcome.

### 1.6 Status glyphs bypass the caps ladder and collide (S)

`AgentDispatchStatus::glyph()` returns hard-coded Unicode
(`crates/thegn-core/src/issue.rs:384-394`):

```rust
Self::Queued | Self::Spawning => "⚙",
Self::Running                 => "⚙",   // <-- three active states, one glyph
```

- **Collision.** Queued, Spawning and Running are indistinguishable — the three
  states a supervisor scans the board for.
- **Ladder bypass.** `⚙ ⏸ ⎇ ✓ ✗` are baked at the source. Nothing resolves them
  through `caps::active_glyphs()` (`crates/thegn-host/src/caps.rs:110-121`), so
  under `[theme] glyphs = ascii` / a non-UTF-8 locale the board mojibakes. The
  glyph ratchet does not catch this: it scans only U+2500–U+259F inside
  `crates/thegn-host/src` (`test/glyph-literal-ratchet.txt` header), and these
  are U+2699/U+23F8/U+2387/U+2713/U+2717 in **core**. The rule in CLAUDE.md
  still applies — the ratchet is a floor, not the contract.
- The value is frozen into `PipelineRow.glyph: &'static str` at fold time
  (`monitor_pipeline.rs:44`, `:226`), so even a caps reload could not move it.

### 1.7 Containers header claims foreign rows (S)

`containers()` heads the whole table `"thegn containers"`
(`monitor/build.rs:905`) — over a list that explicitly includes foreign
containers, marked `" (foreign)"` per row (`build.rs:932`) and counted nowhere.
The owned count is in the note; the *heading* is the lie.

### 1.8 Processes empty state asserts a config value that isn't set (S)

`build::procs` opens with (`monitor/build.rs:788-793`):

```rust
if !snap.enabled {
    return vec![heading("process sampling is off ([monitor] processes = false)", None)];
}
```

`ProcSnapshot` derives `Default` with `enabled: false`
(`crates/thegn-metrics/src/procs.rs:70-83`); only a real sample sets it true
(`procs.rs:216-222`), and `model.procs` is replaced only when one arrives
(`crates/thegn-host/src/run.rs:9495-9498`). The sampler is gated on the tab
being open (`monitor.rs:539-541`, `run.rs:11041`), so **the first frame after
opening Processes always renders the default snapshot** — and tells the user
their config says something it does not. The tab has an honest "sampling…"
state one branch further down (`build.rs:794-798`) that this arm pre-empts.

### 1.9 Help page filed under `bars`; ladder docs drifted (S)

- `docs/help/system-monitor.md:4` — `parent: bars`. The monitor is a
  full-screen modal reachable from a global chord and the palette, not a bar
  feature; burying it under "Masthead & status bar" is why it reads as a
  third-level page. Every other full-surface page (`sidebar`, `panel`,
  `terminal-and-panes`, `search`) is top level.
- `docs/help/system-monitor.md:44` claims the `[`/`]` ladder is
  *"30s, 2m, 10m, 1h, all"*. The shipped ladder is
  `["30s","1m","5m","10m","30m","1h","6h","12h","all"]`
  (`crates/thegn-core/src/series_window.rs:144-145`) and the default window is
  `1m`, not `2m` (`crates/thegn-core/src/config.rs:2697-2700`). `2m` is
  explicitly no longer a rung — a test pins that
  (`monitor/state.rs:314-325`). The docs describe a ladder that has not existed
  for some time.
- `docs/help/system-monitor.md:155-156` documents the missing 10th digit as a
  fact of life; item 5 removes the excuse.

### 1.10 Configured-but-empty stages are invisible (S)

`ordered_rows` groups by the stages **present on the roster**
(`monitor_pipeline.rs:117-146`) and `build::pipeline` walks the produced rows
(`build.rs:1013-1064`). A configured stage with no live rows therefore has no
group and no heading. But `DispatchRoster::is_present()` shows the tab for a
*configured* pipeline with an empty roster (`monitor_pipeline.rs:77-83`) — so
the intended reading is "the board is the org chart", and today the org chart's
empty columns vanish. A Lead cannot see that `review` exists and is idle.

### 1.11 Concurrency / agent / next are not on the board (S)

`[[pipeline.stages]]` carries `agent`, `concurrency`, `next`, `timeout_secs`,
`on_blocked` (`crates/thegn-core/src/config_pipeline.rs:44-73`) — the exact
numbers a supervisor needs beside "2 of 3 active". Only the stage **name**
reaches the UI: `stage_order(cfg)` returns `cfg.pipeline.stage_names()`
(`monitor_pipeline.rs:92-94`), and `DispatchRoster` carries only
`stage_order: Vec<String>` (`monitor_pipeline.rs:69-75`).

---

## 2. Design

Five design decisions carry the change; everything else follows from them.

### D1 — The builder owns row geometry; the overlay only clamps against it

Item 7 needs "where on the stack is row `sel`?". Only the tab builder knows —
Processes puts one table under one heading, Disk puts one under a graph plus two
tables plus a grid, and Pipeline emits one table **per stage group**. Recomputing
that in `monitor.rs` would be a second copy of the layout, which is exactly the
class of drift `sections.rs`'s own doc comment warns about
(`sections.rs:14-17`: *"`Section::height` is load-bearing"*).

So `build::tab` returns a struct, not a `Vec<Section>`:

```rust
pub(super) struct TabBuild {
    pub sections: Vec<Section>,
    /// Stack-relative y of each selectable row, in `sel` order. Empty on a
    /// tab with no row cursor.
    pub row_y: Vec<usize>,
}
```

Each list builder stamps `row_y` with a two-line helper, measured with the same
function the scroll clamp uses:

```rust
/// Stack-relative y of the `n` body rows of a table about to be pushed onto
/// `out`. Measured with `sections::stack_height`, the same function
/// `scroll_max` measures against — so the cursor and the clamp can never
/// disagree about where a row is.
fn row_ys(out: &[Section], n: usize, has_header: bool) -> Vec<usize> {
    let base = crate::sections::stack_height(out) + has_header as usize;
    (base..base + n).collect()
}
```

The overlay stores `row_y: Vec<usize>` and gains:

```rust
/// Scroll the minimum distance that brings `sel` into the viewport.
fn follow_row(&mut self) { … }
```

`nav()` on a list tab moves **only** the cursor and lets `follow_row` place the
viewport; on a graph tab it scrolls as today. That is the safety fix: after it,
what `x` targets is by construction the row that is highlighted and on screen.

A `follow: bool` (default `true`) is cleared by `wheel()` and set by every
cursor-moving key, so a wheel-scrolled viewport is not yanked back on the next
live refresh, and the first `j` re-arms following. One bool, no mode.

### D2 — Selection is a `TableSection` property, painted once in `draw_table`

Rather than four copies of "tint the name cell", `TableSection` gains
`sel: Option<usize>` and `draw_table` paints the selected row: a
`half_block_r` gutter in `S::Accent`, every cell `.bg(Tok::Slot(S::Panel2))`,
and `S::Panel2` as the `put_line` pad so the tint runs the full row width.
Unselected rows in a table that *has* a selection get a one-space gutter, so
columns stay aligned. This kills the Containers regression for free: the
ownership tint stays the foreground and the selection is the background, so the
two stop fighting over one cell.

`table_cols` (`sections.rs:548-551`) must add the gutter when `sel.is_some()` —
it is what callers size containers with.

### D3 — Tab-bar windowing is a pure function in its own module

`crates/thegn-host/src/monitor.rs` is 1518 lines and is on the "don't grow the
god-files" list. New chrome goes in siblings under `monitor/`:

- `monitor/tabbar.rs` — `digit(i)` (`0..=8 → '1'..='9'`, `9 → '0'`) and
  `window(widths, active, width) -> TabWindow` (the contiguous run of tabs that
  fits, always containing `active` **whole**, with left/right overflow flags).
  Pure, no `self`, unit-tested in place.
- `monitor/footer.rs` — the footer `Line` builder lifted out of
  `monitor.rs::footer`, taking a small explicit input struct.

Overflow markers use `Glyph::QuoteOpen` / `Glyph::QuoteClose` (`«`/`»`,
ASCII `<`/`>` — `crates/thegn-core/src/termcaps.rs:466-467`, `:528-529`). They
resolve through `caps::glyph()`, so no new core vocabulary is minted for a
chrome affordance and the ASCII ladder degrades for free. **No glyph literal at
a draw site** — that rule is not negotiable here even though the ratchet's
U+2500–U+259F window would not have caught one.

### D4 — One status-glyph token vocabulary, resolved at draw time

`thegn-core` owns the *token*; the host owns the *resolution*.

```rust
// crates/thegn-core/src/issue.rs
impl AgentDispatchStatus {
    /// The glyph token for this status — resolved against the live glyph set
    /// at the DRAW site, so the board degrades with `[theme] glyphs`.
    /// One token per PHASE, not per variant: `Merged`/`Done` are both "finished
    /// cleanly", `Abandoned`/`Failed` both "ended badly". The five ACTIVE
    /// states are pairwise distinct at every glyph level — they are what a
    /// supervisor scans for.
    pub fn glyph_token(self) -> Glyph { … }

    /// Unchanged shape for the CLI (`thegn dispatch list`), now defined as the
    /// token resolved at full Unicode, so the two can never drift.
    pub fn glyph(self) -> &'static str { self.glyph_token().resolve(&termcaps::UNICODE) }
}
```

| status | token | Full | ASCII |
|---|---|---|---|
| Queued | `DiamondHollow` | `◇` | `o` |
| Spawning | `Refresh` | `↻` | `@` |
| Running | `DotFilled` | `●` | `*` |
| WaitingHuman | `Attention` | `✋` | `!` |
| PrOpen | `Hex` | `⬡` | `#` |
| Merged, Done | `Check` | `✓` | `+` |
| Abandoned, Failed | `Cross` | `✗` | `x` |
| Unknown | `DotHollow` | `○` | `o` |

`Unknown` shares `o` with `Queued` at the ASCII rung only; both are drawn
immediately beside `status.as_str()` (`build.rs:1045`), and `Unknown` means "a
string this build cannot parse", which is rare and already labelled. Accepting
that is cheaper than growing the core `GlyphSet` for it.

`PipelineRow.glyph` is **deleted**. The row already carries `status`, and a
`&'static str` frozen at fold time cannot follow a caps reload — `build.rs`
calls `crate::caps::glyph(r.status.glyph_token())` at the draw site. This also
keeps `ordered_rows` pure of any caps read, which its module doc requires
(`monitor_pipeline.rs:1-7`).

### D5 — The roster carries stage *metadata*, not just names

`DispatchRoster.stage_order: Vec<String>` becomes
`stages: Vec<StageMeta>` — one source, not two:

```rust
/// A configured stage as the board displays it. A projection of
/// `PipelineStage`, NOT a re-export: the board shows what a supervisor reads
/// off the org chart, and `[[pipeline.stages]]` is structure-not-judgment
/// (`config_pipeline`'s doctrine) — nothing here is enforced by thegn.
pub(crate) struct StageMeta {
    pub name: String,
    pub agent: String,
    pub concurrency: u32,
    pub next: Option<String>,
}

impl DispatchRoster {
    pub fn stage_names(&self) -> Vec<String> { … }   // what `ordered_rows` takes
}
```

`ordered_rows`'s signature is untouched (`&[String]`), so its whole test corpus
survives. Sampled off-loop exactly as before —
`monitor_action::spawn_dispatch_sample` already carries the config-derived
value across the thread boundary (`monitor_action.rs:234-258`), so this adds no
wake source and no loop-side config read.

`build::pipeline` then:
- draws each **configured** stage in configured order, whether or not it has
  rows; an empty one gets its heading plus a dim `idle` note rather than
  vanishing (item 1.10);
- puts the stage's `agent · max N · → next` in the heading note beside
  `n of m active` (item 1.11);
- then draws the row-only stages and `unstaged`, which `ordered_rows` already
  emits after the configured ones (`monitor_pipeline.rs:130-145`) — so `sel`
  indexing is unchanged.

### D6 — `Enter` opens the worktree; the resolution stays pure

`pipeline_target` is widened into a pure three-way resolution and the loop
executes it:

```rust
pub enum PipelineLanding {
    /// A sidebar row already targets it — the existing door, unchanged.
    Row(crate::sidebar::RowTarget),
    /// Registered in the DB but not resident in this session: open it as a
    /// group. The tab's `CenterTree::Leaf(0)` is a missing leaf, so the lazy
    /// materialize path spawns its pane — the same path a freshly created
    /// worktree takes.
    Open { tab_name: String, path: String },
    /// Nothing known — the existing notice.
    None,
}

pub fn pipeline_landing(jump: &PipelineJump, model: &FrameModel) -> PipelineLanding
```

`Row` is tried first, so nothing about the existing (correct) case changes.
`Open` is looked up in `model.sidebar_db_worktrees` by `path == jump.worktree`
(`chrome.rs:458`, `sidebar.rs:476-492`). Pure over the model, so both arms are
unit-testable beside the existing `pipeline_tests` module
(`monitor_action.rs:287-334`).

The loop's `Open` arm mirrors `creating::open_tab`
(`handlers/creating.rs:81-95`): `add_group`, `refresh_tab_model`,
`need_relayout = true`, close the monitor, focus Center. It must be idempotent
on the group name — `add_group` after a `position(|g| g.name == tab_name)`
check, switching to the existing group when one is there.

Routing this through `RowTarget::Workspace` was rejected: `switch_workspace`
returns early when the target IS the current workspace and its `land_on` is a
no-op for a non-resident group (`run.rs:1976-1987`), which is the same silent
dead end in a different costume.

### D7 — The footer's help door is an outcome, not a passthrough

`?` / `F1` return a new `MonitorOutcome::Help`; the loop opens
`help::open_at(&reg, &cfg, "overlay:monitor")`. Passthrough was rejected:
`help::open` resolves the page from **focus zone / panel section**
(`help/context.rs:26-31`), and the monitor is neither, so the global key would
land on whatever is focused behind the modal. The help overlay already renders
*after* the monitor (`run.rs:12018-12036`), so it stacks correctly with the
monitor left open behind it.

`contexts:` entries are validated against `help::context::vocabulary()`
(`help/context.rs:33-58`), so `"overlay:monitor"` must be added there and
claimed by `docs/help/system-monitor.md`. It is claimed in the same change, so
`test/help-context-ratchet.txt` cannot grow.

---

## 3. Invariants this change must not break

- **0% idle.** Nothing here adds a timer, thread or channel. `follow_row` is
  arithmetic inside an existing rebuild; `StageMeta` rides the existing off-loop
  roster sample; the `Open` arm runs on a keystroke.
- **Render decision is pure.** No change to `render_plan`. All of this is chrome
  composed inside an existing `Full` frame.
- **Degrade at the edges.** Every new glyph goes through `caps::glyph()` /
  `active_glyphs()`; every new color is a `Tok::Slot` / `Tok::Hue`. **No literal
  at a draw site**, in `sections.rs`, `monitor/*` or `thegn-core` — neither file
  set is on `test/glyph-literal-ratchet.txt` or
  `test/color-literal-ratchet.txt`, and both ratchets are shrink-only.
- **`thegn-core` stays substrate-free and 95%-line covered.** The only core edit
  is `AgentDispatchStatus::glyph_token` — pure, and it needs a unit test in
  `issue.rs`'s existing `spec` module or coverage regresses.
- **`Section::height` must equal what is drawn.** The selection gutter is a
  *horizontal* addition; it must not change any section's row count.
- **Help ratchets.** No new `ACTION_SPECS` id is introduced (the monitor's
  internal keys are overlay-local, not bindable actions), so
  `test/help-ratchet.txt` and `test/help-prose-ratchet.txt` are untouched. The
  help page must still mention what it claims — the `actions:` list
  (`open-monitor`, `open-pipeline-board`) stays and stays described.
- **Ignored `Result`s.** No new `let _ =` without a `// best-effort:` reason.

## 4. Testing

`just quick thegn-host` / `just quick thegn-core` while iterating; scoped
`cargo nextest run -p <crate> <filter>` for the tests each chunk adds. **No
full-workspace gate in these chunks** — `just test` / `just ci` are the Lead's
pre-PR run.

**e2e is deliberately not run.** Every chunk changes drawn frames (tab bar,
table rows, footer, board headings), so all 45 baselines under
`test/muse/snapshots/` are stale after this lane. Per CLAUDE.md e2e is a known
-broken/opt-in gate and the baselines were already stale before this change
(last recorded `0f9c5a9a`); re-recording with `just e2e-update` is a follow-up
for whoever revives that gate, and is called out here so it is not mistaken for
an oversight.

## 5. Chunking

Three chunks, **strictly serial in order 1 → 2 → 3**. They are not file-disjoint
and cannot be: `monitor.rs` and `monitor/build.rs` are the subject of the whole
lane, and both appear in all three. Do not run them in parallel.

| # | Theme | Items |
|---|---|---|
| 1 | Tab bar, row cursor, viewport-follows-selection | 5, 6, 7 |
| 2 | Board & empty-state honesty | glyphs, empty stages, stage meta, containers heading, procs empty state |
| 3 | `Enter` opens the worktree; footer hints + help door; help page | 10, 8, docs |

Chunk specs: `.thegn/pipeline/THE-75/code/chunk-{1,2,3}.md`.

## 6. Explicitly out of scope

Per the issue, and not to be churned: item 9 (board v2 single-table layout —
THE-74); the pause model; visibility gating; the coverage note; the signal
confirm flow; `ordered_rows` purity and its tests; row-identity caching;
wheel + outside-click handling (chunk 1 touches `wheel()` only to clear the
follow flag — its scroll behaviour is unchanged). Item 1 (`parse_chord` Shift
synthesis) belongs to the THE-70 lane; do not touch keymap chord parsing.
