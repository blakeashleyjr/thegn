# THE-75 chunk 2 — done

Branch `tg/the-75-monitor-fixes`. Covers the S-effort findings **1.6**, **1.7**,
**1.8**, **1.10**, **1.11** (design §D4–§D5).

Commits (oldest first):

| sha        | subject                                                                                                   |
| ---------- | --------------------------------------------------------------------------------------------------------- |
| `42e7adee` | `wip(monitor): AgentDispatchStatus::glyph_token — one status vocabulary through the caps ladder (THE-75)` |
| `06e4c404` | `fix(monitor): caps-ladder status glyphs, the whole org chart, and honest empty states (THE-75)`          |

The final commit subject is the exact string the chunk spec required.

## What landed

### 1. One status-glyph token vocabulary (`thegn-core/src/issue.rs`)

- `AgentDispatchStatus::glyph_token() -> termcaps::Glyph`, exactly the §D4 table:
  `Queued→DiamondHollow`, `Spawning→Refresh`, `Running→DotFilled`,
  `WaitingHuman→Attention`, `PrOpen→Hex`, `Merged|Done→Check`,
  `Abandoned|Failed→Cross`, `Unknown→DotHollow`.
- `glyph()` is now _defined_ as `glyph_token().resolve(&termcaps::UNICODE)` —
  the CLI (`thegn dispatch list`) and the board cannot drift.
- No glyph literal was added anywhere. The pre-existing `⚙ ⏸ ⎇ ✓ ✗` literals in
  `issue.rs` are **gone** — including from the test, which now pins the
  `(status, wire string, Glyph token)` triple via a `STATUS_TOKENS` table rather
  than expected strings.

Core tests: `agent_dispatch_status_string_representations` (rewritten to assert
the token), plus the two new ones —
`glyph_agrees_with_its_token` and
`every_active_status_has_a_distinct_glyph_at_every_rung` (pairwise-distinct
against **both** `UNICODE` and `glyphs(UnicodeLevel::Ascii)`; asserts the active
set is exactly 5).

### 2. `PipelineRow.glyph` deleted (`monitor_pipeline.rs`)

Field and its assignment in `row()` are gone; the row carries only `status`.
`build.rs` resolves at the draw site with
`crate::caps::glyph(r.status.glyph_token())`, so `ordered_rows` stays free of
any caps read (its module doc's requirement).
`row_fields_carry_glyph_basename_and_age` → renamed
`row_fields_carry_status_basename_and_age`, asserting `r.status`.

### 3. The roster carries stage metadata (`monitor_pipeline.rs`)

- New `StageMeta { name, agent, concurrency, next }` and
  `stage_meta(cfg) -> Vec<StageMeta>`, skipping unnamed/blank stages via
  `stage_name()` / `next_name()` (so a blank `next` is terminal, not a stage
  called `""`).
- `DispatchRoster.stage_order: Vec<String>` → `stages: Vec<StageMeta>`, plus
  `stage_names()`. `is_present()` reads `!stages.is_empty()`.
- `stage_order(cfg)` deleted.
- **`ordered_rows`'s signature is untouched** (`&[String]`) — its entire test
  corpus passes unmodified.
- Wiring: `monitor.rs` → `&model.dispatches.stage_names()`;
  `monitor_action::spawn_dispatch_sample` takes `stages: Vec<StageMeta>` (still
  the one off-loop `spawn_blocking` pulsing the existing waker — **no new wake
  source, no loop-side config read**); `run.rs` passes
  `monitor_pipeline::stage_meta(&current_config)`.
- New test `stage_meta_projects_named_stages_in_declaration_order` (trimming, the
  half-written entry, blank-`next`-is-terminal, and `stage_names()` agreement).

### 4. The board shows the whole org chart (`monitor/build.rs::pipeline`)

`pipeline(rows, stages, sel)`; `stages` plumbed through `TabInput.pipeline_stages`
from `model.dispatches.stages`.

- Configured stages are walked **first, in configured order**, whether or not
  the roster has rows. A staffed stage draws its table as before; an idle one
  draws its heading with an `idle …` note and **no table**.
- Then the remaining row-groups in row order (renamed/discovered stages and
  `unstaged`), which `ordered_rows` already emits after the configured ones — so
  `sel` indexing and the `row_y` runs are unchanged.
- Heading note: `{live} of {n} active` (or `idle`) plus, for a configured stage,
  ` · {agent} · max {N} · {›} {next}`; the hand-off is omitted for a terminal
  stage and the whole suffix for a discovered/unstaged group. One line.
- The `"no dispatches yet"` early return now requires **both** `rows.is_empty()`
  and `stages.is_empty()`.
- Extracted `stage_table()` (table + `row_y` run + group `sel`), `stage_run()`
  (span of one stage label) and `stage_meta_note()` so the two walks share one
  implementation instead of a copy.

### 5. Containers heading tells the truth (`monitor/build.rs::containers`)

Heading is now `"containers"`; the note leads with `{owned} owned · {foreign}
foreign`, then either the footprint fields (`img`/`vol`/`≥ engine disk`, `≥`
marker unchanged) or `{running} running`.

### 6. Processes empty state stops asserting the config

- `FrameModel` gains a config-derived flag beside `disk_warn_threshold_gb`,
  set in `build_model` (`hydrate.rs`) from `app_cfg.monitor.processes`, and
  added to `hydration_eq` so a config reload that toggles it repaints.
- `build::procs` is three branches: config-off → `[monitor] processes = false`;
  gate open but `!snap.enabled || snap.procs.is_empty()` → `sampling…`; else the
  table. The snapshot can no longer speak for the config.

## Tests

All six host tests from the spec, in `monitor_tests.rs`:

| #   | test                                                             |
| --- | ---------------------------------------------------------------- |
| 1   | `a_configured_stage_with_no_rows_still_appears_on_the_board`     |
| 2   | `a_stage_heading_carries_its_agent_concurrency_and_next`         |
| 3   | `an_empty_roster_with_a_configured_pipeline_draws_the_org_chart` |
| 4   | `board_row_glyphs_degrade_to_ascii`                              |
| 5   | `the_containers_heading_does_not_claim_foreign_rows`             |
| 6   | `an_unsampled_processes_tab_says_sampling_not_disabled`          |

Plus the core pair (§1) and `stage_meta_projects_named_stages_in_declaration_order`.
New helpers: `headings`, `stage_headings` (drops `spacer()`, which is itself an
empty heading), `board_row`, `model_with_org_chart`, `stage(name, next)`.

### What was run (scoped only, per the dev-loop policy)

```
just quick thegn-core                                  clean
just quick thegn-host                                  clean (clippy -D warnings)
cargo nextest run -p thegn-core issue                  51/51 pass
cargo nextest run -p thegn-core config                 541/541 pass
cargo nextest run -p thegn-host monitor --no-fail-fast 94/94 pass
cargo nextest run -p thegn-host ratchet                12/12 pass
cargo nextest run -p thegn-host model_eq                3/3 pass
cargo nextest run -p thegn-host hydrate                73/73 pass
```

The ratchet run covers `glyph_literals_go_through_active_glyphs` and
`color_literals_stay_in_the_chokepoints` — green with **no new allowlist
entries**. No new `let _ =` / `.ok()`. Every pre-existing `monitor_pipeline`
test passes unmodified except the two the spec named.

## Deviations from the spec (deliberate)

1. **`procs_enabled` is stored inverted, as `FrameModel.procs_disabled`, and
   read through `FrameModel::procs_enabled()`.** The spec asks for
   `pub procs_enabled: bool` defaulting to `true`, but `FrameModel` is
   `#[derive(Default)]` over ~120 fields: a `bool` field defaults to `false`,
   Rust has no field-level default for structs, and hand-writing `Default` for
   that struct is not a change worth making here. A `false` default would mean
   every frame before the first `build_model` renders
   `"[monitor] processes = false"` — exactly the lie this fix exists to remove.
   Inverting makes the derive's default _enabled_, matching
   `MonitorConfig::default().processes`. The field doc states this; the accessor
   keeps the spec's name at every read site. Flagging it because it is a
   spec-named field.
2. **The hand-off marker is `Glyph::Chevron` (`›` / ASCII `>`), not `→`.** Core
   mints no right-arrow token and this is a draw site, so a literal `→` was not
   an option and minting a new core glyph is outside this chunk. Test 2 asserts
   on the resolved chevron; a terminal stage's note contains none.
3. **`model_eq.rs` and `hydrate.rs` were touched** — not in the spec's file
   table, but "mirror the `disk_warn_threshold_gb` site exactly" points at
   `hydrate::build_model` (the spec says `run.rs`; the assignment is actually in
   `hydrate.rs:2440`, called from the loop), and `hydration_eq` is the other half
   of that site — without it a config reload toggling `[monitor] processes` would
   be swallowed by the idle guard.
4. **`fp.containers.max(owned)` is dropped from the Containers note.** The spec
   prescribes `{owned} owned · {foreign} foreign` from the list, then the
   footprint fields; the engine-reported container count no longer appears. If
   the engine's own count mattered (it could exceed the listed owned count), that
   is a one-line restore.

## Notes for review

- A configured stage literally named `unstaged` would draw twice (idle at its
  config position, then as the trailing `UNSTAGED` group), because `ordered_rows`
  always emits `UNSTAGED` last. Rows still appear exactly once and `sel` indexing
  is unaffected (the idle branch consumes no rows). Pathological config; left
  alone rather than special-cased.
- The `"no dispatches yet"` early return is now only reachable when the tab is
  drawn with neither rows nor stages, which `DispatchRoster::is_present` already
  prevents — kept as a defensive branch, so it is uncovered by test.

## Unverified

- **No full-workspace gate was run** (`just test`, `just ci`, `just coverage`,
  `just lint`), per the chunk spec and the Lead addenda. `thegn-core` coverage
  (the 95% line gate) is therefore unverified: `glyph_token` is exercised by
  three tests and `glyph()` by everything that already called it, so the gate
  should be unaffected, but it was not measured.
- **`just e2e` was not run.** As chunk 1 recorded, every chunk in this lane
  changes drawn frames — this one changes the Containers heading text, the board's
  status glyphs and stage headings — so the 45 baselines under
  `test/muse/snapshots/` are stale for this branch. Re-recording with
  `just e2e-update` is a follow-up for whoever revives that gate.
- **Tests outside the `monitor` / `ratchet` / `model_eq` / `hydrate` / `issue` /
  `config` filters were not run.** A whole-crate `cargo nextest run -p thegn-host`
  is the Lead's pre-push gate. `chrome_tests` in particular constructs
  `FrameModel` in many places; the new field has a `Default`, so nothing there
  needed editing, but that was reasoned rather than executed.
- **The Chevron hand-off marker was not eyeballed in a real terminal**, only
  asserted in tests. Its width-1 ASCII fallback (`>`) is from the shared table.
- A stray whole-tree `treefmt` run during commit reformatted five
  `.thegn/pipeline/**` markdown files; those were reverted with `git restore` and
  are **not** in either commit. No source file outside this chunk's table was
  left modified (`git status` is clean at the final commit).
