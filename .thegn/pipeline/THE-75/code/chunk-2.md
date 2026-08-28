# THE-75 chunk 2 — Caps-ladder status glyphs, the whole org chart, and honest empty states

Read `.thegn/pipeline/THE-75/architect/design.md` §1.6–§1.8, §1.10–§1.11 and
§D4–§D5 first — the evidence and the rationale are there; this file is the work
order.

Covers the S-effort B/C findings: status glyphs bypass the caps ladder and
collide; Containers header says "thegn containers" over foreign rows; the
Processes empty state asserts a config value that isn't set; configured-but-empty
stages are invisible; concurrency/agent/next are not shown on the board.

## Ordering / overlap

- **Runs SECOND. Serial.** Depends on chunk 1: `build::tab` must already return
  `TabBuild` and `TableSection` must already carry `sel`. Do not start before
  chunk 1 has landed on the branch.
- Chunk 3 also edits `monitor.rs`, `monitor_action.rs` and `run.rs`. Do not run
  in parallel with it.

## Files touched (exact)

| Path                                        | Why                                                                                                                     |
| ------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------- |
| `crates/thegn-core/src/issue.rs`            | `AgentDispatchStatus::glyph_token`; `glyph()` redefined through it; its tests                                           |
| `crates/thegn-host/src/monitor_pipeline.rs` | `StageMeta`, `DispatchRoster.stages`, `stage_meta(cfg)`, drop `PipelineRow.glyph`                                       |
| `crates/thegn-host/src/monitor/build.rs`    | Draw-time glyph resolution, empty configured stages, stage-meta heading note, Containers heading, Processes empty state |
| `crates/thegn-host/src/monitor.rs`          | `ordered_rows` call site now takes `stage_names()`                                                                      |
| `crates/thegn-host/src/monitor_action.rs`   | `spawn_dispatch_sample` carries `Vec<StageMeta>`                                                                        |
| `crates/thegn-host/src/chrome.rs`           | `FrameModel.procs_enabled`                                                                                              |
| `crates/thegn-host/src/run.rs`              | The two roster/config wiring sites (see below)                                                                          |
| `crates/thegn-host/src/monitor_tests.rs`    | New tests                                                                                                               |

Do **not** touch: `sections.rs`, `docs/help/`, `monitor/tabbar.rs`, anything in
`handlers/`.

## Work

### 1. One status-glyph token vocabulary (`thegn-core/src/issue.rs`)

Replace the hard-coded `glyph()` (`issue.rs:384-394`) with a token + a
resolution:

```rust
/// The glyph token for this status, resolved against the live glyph set at
/// the DRAW site — so the board degrades with `[theme] glyphs` / a non-UTF-8
/// locale instead of mojibaking. (A `&'static str` baked in here could not
/// follow a caps reload, which is why the board used to.)
///
/// One token per PHASE, not per variant: `Merged`/`Done` are both "finished
/// cleanly" and `Abandoned`/`Failed` both "ended badly". What must never
/// collide is the five ACTIVE states — they are what a supervisor scans a
/// board for, and `Queued`/`Spawning`/`Running` all rendered `⚙` before this.
pub fn glyph_token(self) -> crate::termcaps::Glyph {
    use crate::termcaps::Glyph as G;
    match self {
        Self::Queued => G::DiamondHollow,
        Self::Spawning => G::Refresh,
        Self::Running => G::DotFilled,
        Self::WaitingHuman => G::Attention,
        Self::PrOpen => G::Hex,
        Self::Merged | Self::Done => G::Check,
        Self::Abandoned | Self::Failed => G::Cross,
        Self::Unknown => G::DotHollow,
    }
}

/// The full-Unicode glyph — what `thegn dispatch list` prints. Defined as
/// [`Self::glyph_token`] resolved at the top rung so the CLI and the board can
/// never disagree about what a row is doing.
pub fn glyph(self) -> &'static str {
    self.glyph_token().resolve(&crate::termcaps::UNICODE)
}
```

The existing `glyph()` assertions in `issue.rs`'s `spec` module (around
`:513` and `:635`) will fail — update the expected strings to match the table in
the design (§D4). **`thegn-core` is 95%-line gated**, so add:

- `every_active_status_has_a_distinct_glyph_at_every_rung` — the five active
  statuses resolve pairwise-distinct against **both** `termcaps::UNICODE` and
  the ASCII set (use `termcaps::glyphs(UnicodeLevel::Ascii)`).
- `glyph_agrees_with_its_token` — for every variant,
  `s.glyph() == s.glyph_token().resolve(&UNICODE)`.

No literal glyph anywhere in this edit.

### 2. `PipelineRow.glyph` is deleted (`monitor_pipeline.rs`)

- Remove the `glyph: &'static str` field (`monitor_pipeline.rs:44`) and its
  assignment in `row()` (`:226`). The row already carries `status`; freezing a
  resolved string at fold time is what made the board caps-blind, and
  `ordered_rows` must stay free of any caps read (its module doc,
  `monitor_pipeline.rs:1-7`).
- `build.rs` draws it as `crate::caps::glyph(r.status.glyph_token())`
  (`build.rs:1045`).
- Update `row_fields_carry_glyph_basename_and_age`
  (`monitor_pipeline.rs:607-621`) to assert on `r.status` instead.

### 3. The roster carries stage metadata (`monitor_pipeline.rs`)

```rust
/// A configured stage as the board displays it — a projection of
/// [`thegn_core::config::PipelineStage`], not a re-export. `[[pipeline.stages]]`
/// is STRUCTURE, NOT JUDGMENT (`config_pipeline`'s doctrine): nothing here is
/// enforced by thegn, it is what a supervising agent reads off the org chart,
/// shown where the agent is already looking.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct StageMeta {
    pub name: String,
    pub agent: String,
    pub concurrency: u32,
    pub next: Option<String>,
}

/// The configured stages, in declaration order. Replaces `stage_order`.
pub(crate) fn stage_meta(cfg: &thegn_core::config::Config) -> Vec<StageMeta>
```

- Skip unnamed/blank stages exactly as `Pipeline::stage_names` does
  (`config_pipeline.rs:139-144`) — use `stage_name()` / `next_name()` so a
  half-written entry never opens a phantom column.
- `DispatchRoster.stage_order: Vec<String>` → `stages: Vec<StageMeta>`, plus
  `pub fn stage_names(&self) -> Vec<String>`. **`ordered_rows`'s signature is
  unchanged** (`&[String]`) — its whole test corpus must survive untouched.
- `is_present()` (`:80-83`) becomes `!self.rows.is_empty() || !self.stages.is_empty()`.
- Delete `stage_order(cfg)` (`:92-94`) — `stage_meta` supersedes it.
- Update `roster_presence_gates_the_tab` (`:666-684`) and the
  `monitor_tests.rs` fixture (`monitor_tests.rs:75-89`) to the new field.

Wiring, three call sites:

- `monitor.rs:609-613` → `&model.dispatches.stage_names()`.
- `monitor_action.rs:234-258` → the parameter becomes `stages: Vec<StageMeta>`
  and the constructed roster uses it. Still one off-loop `spawn_blocking` task
  that pulses the existing waker — **no new wake source**.
- `run.rs:9583-9587` → pass `crate::monitor_pipeline::stage_meta(&current_config)`.

### 4. The board shows the whole org chart (`monitor/build.rs::pipeline`)

`pipeline` takes the stage list as well as the rows:

```rust
fn pipeline(rows: &[PipelineRow], stages: &[StageMeta], sel: usize) -> (Vec<Section>, Vec<usize>)
```

(plumb `stages` through `TabInput` from `model.dispatches.stages`).

- Walk the **configured** stages first, in configured order. For each: draw its
  heading; if the roster has a group for it, draw that group's table as today;
  if it has none, draw a dim `idle` note and **no table** — a configured stage
  with nothing running is a fact about the pipeline, not an absence
  (`DispatchRoster::is_present` already shows the tab for a configured-but-never
  -run pipeline, so the empty column vanishing was the inconsistency).
- Then walk the remaining row-groups in row order — `ordered_rows` already emits
  unknown stages and `unstaged` **after** the configured ones
  (`monitor_pipeline.rs:130-145`), so `sel` indexing is unchanged and the
  `row_y` runs stay in `sel` order.
- Heading note: keep `n of m active` and add the stage's own numbers when it is
  a configured stage — `agent · max N · → next`, with `→ next` omitted for a
  terminal stage and the whole suffix omitted for a discovered/unstaged group.
  Keep it one line; `Section::Heading`'s note is right-aligned and will be cut
  by `draw_line` on a narrow box, which is the correct degradation.
- Guard the top-line summary: with rows empty but stages configured, the
  `"no dispatches yet"` early return (`build.rs:1008-1011`) must **not** fire —
  draw the idle org chart instead. Keep the early return for the genuinely
  empty case (no rows, no stages).

### 5. Containers heading tells the truth (`monitor/build.rs::containers`)

`build.rs:905` heads a mixed list `"thegn containers"`. Change the heading to
`"containers"` and put the ownership split in the note: `{owned} owned`,
`{foreign} foreign` (count `list.iter().filter(|c| !c.ours)`), then the existing
footprint fields when `container_footprint` is present. Keep the `≥` partial
marker (`:899`) as-is.

### 6. Processes empty state stops asserting the config (`chrome.rs`, `run.rs`, `build.rs`)

- `FrameModel` gains, beside the other config-derived fields
  (`chrome.rs:467` `disk_warn_threshold_gb` is the precedent):

```rust
/// `[monitor] processes` — whether the Processes tab is enabled at all.
/// Mirrored onto the model because `ProcSnapshot::default()` has
/// `enabled: false`, so the tab could not tell "the user turned it off" from
/// "the first sample has not landed yet" and told the user their config said
/// something it did not.
pub procs_enabled: bool,
```

Default `true` (matching `MonitorConfig::default().processes`,
`config.rs:2704`). Set it where the config is loaded **and** on reload,
wherever `model.disk_warn_threshold_gb` is assigned in `run.rs` — mirror that
site exactly, do not invent a second one.

- `build::procs` (`build.rs:788-798`) becomes three honest branches:

```rust
if !cx.model.procs_enabled {
    return vec![heading("process sampling is off ([monitor] processes = false)", None)];
}
if !snap.enabled || snap.procs.is_empty() {
    // The gate opened but no sample has landed yet — `ProcSnapshot::default()`
    // is what the model holds on the first frame after the tab opens.
    return vec![heading("sampling…", None)];
}
```

## Tests

`crates/thegn-core/src/issue.rs` — the two tests in §1 above, plus the updated
existing assertions.

`crates/thegn-host/src/monitor_tests.rs`:

1. `a_configured_stage_with_no_rows_still_appears_on_the_board` — a roster with
   `stages = [architect, code, review]` and rows only under `code` draws three
   headings, in configured order, with `review` marked idle and carrying no
   table.
2. `a_stage_heading_carries_its_agent_concurrency_and_next` — the note contains
   the agent name, the concurrency, and the `next` target; a terminal stage's
   note has no `→`.
3. `an_empty_roster_with_a_configured_pipeline_draws_the_org_chart` — no
   `"no dispatches yet"`, one heading per configured stage.
4. `board_row_glyphs_degrade_to_ascii` — build the Pipeline tab under
   `caps::test_override::with_unicode(UnicodeLevel::Ascii, …)`
   (`caps.rs:170-176`) and assert the status cells hold the ASCII forms and that
   the five active statuses are still pairwise distinct.
5. `the_containers_heading_does_not_claim_foreign_rows` — with one owned and one
   foreign container the heading is `"containers"` and the note reports both
   counts.
6. `an_unsampled_processes_tab_says_sampling_not_disabled` — `procs_enabled =
true` with a default `ProcSnapshot` renders `"sampling…"`; only
   `procs_enabled = false` renders the `[monitor] processes = false` line.

Run, scoped only:

```sh
just quick thegn-core
just quick thegn-host
cargo nextest run -p thegn-core issue
cargo nextest run -p thegn-host monitor
```

Do **not** run `just test`, `just ci`, `just coverage`, or `just e2e`.

## Done criteria

- All six host tests and both core tests pass; every pre-existing
  `monitor_pipeline` test still passes **unmodified** except
  `row_fields_carry_glyph_basename_and_age` and `roster_presence_gates_the_tab`.
- `just quick thegn-core` and `just quick thegn-host` are clean.
- `thegn-core` gained no substrate dependency and no glyph literal; the new
  `glyph_token` is covered (the 95% gate).
- No new wake source, no config read on the event loop: `stage_meta` is read at
  the existing `spawn_dispatch_sample` call site and travels off-thread.
- Commit with **exactly** this subject:

```
fix(monitor): caps-ladder status glyphs, the whole org chart, and honest empty states (THE-75)
```
