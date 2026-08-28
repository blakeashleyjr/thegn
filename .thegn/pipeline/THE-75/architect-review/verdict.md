# THE-75 — architect review of the implementation

Branch `tg/the-75-monitor-fixes`, reviewed at `06f63ed2` (post-merge of `main`).
Design: `.thegn/pipeline/THE-75/architect/design.md`. Chunks 1–3, each with a
done report; every `Unverified` section in those reports was either verified
here or dispositioned below.

**Verdict: APPROVED.** Two small corrections were applied by the reviewer
(commits `bfdea3bd`, `06f63ed2`); no revision chunk is needed.

---

## 0. Lead addenda — merge of main

`main` had moved (THE-70 sidebar/doctor, THE-83 agents/model/env, bundled
skills). Merged into the branch as `55e9f0ac`. One conflict:
`docs/help/system-monitor.md` — THE-75's idle-stage/org-chart paragraph vs
THE-83's `/pipeline`-skill paragraph, both kept (they describe different
things and read correctly in sequence). Auto-merges elsewhere; `spawn_dispatch_sample`'s
call site carries `stage_meta(&current_config)` intact. `just quick thegn-host`
and `just quick thegn-core` are clean on the merged tree.

## 1. Design conformance

- **§D1 (row cursor)** — `TableSection.sel` + gutter + `S::Panel2` background +
  full-width pad, exactly as specified; header reserves the gutter; `table_cols`
  +1; **`Section::height` for `Table` verified unchanged** (header + rows).
  The containers regression is fixed: `Hue::Green`/`S::Ghost` is again the sole
  foreground rule, and a regression test pins it.
- **§D2 (tab bar)** — `tabbar.rs` is pure and tested; `digit`/`index_of` are one
  table (`index_of` asks `digit`); `0` reaches the tenth tab; the window grows
  right-then-left from the active tab and never drops it; overflow markers are
  `QuoteOpen`/`QuoteClose` through `caps::glyph`. The both-marker over-reserve
  is documented and safe (never under-reserves).
- **§D3 (viewport follows cursor)** — `row_y` comes from the builder measured
  with `sections::stack_height` (same measure as `scroll_max`), `follow_row`
  runs after `clamp()` in `rebuild`, and the loop rebuilds on every
  non-passthrough outcome, so `nav()`'s sel-only move is followed immediately.
  `wheel()` clears `follow`; cursor keys re-arm. The `x`-retarget safety test
  is in place.
- **§D4 (glyph tokens)** — `glyph_token()` matches the §D4 table token for
  token; `glyph()` is defined as the token at full Unicode; `PipelineRow.glyph`
  deleted; draw site resolves `caps::glyph(status.glyph_token())`, so
  `ordered_rows` stays caps-free. Distinctness of the five active states is
  tested at **both** rungs. All `⚙ ⏸ ⎇ ✓ ✗` literals are gone from `issue.rs`.
- **§D5 (stage meta)** — `StageMeta` projection + `stage_names()` derived, not
  stored; `ordered_rows` signature untouched; the board walks configured stages
  first (idle ones get a heading), then roster-only groups; per-group `row_y`
  runs concatenate in row order, so global `sel` indexing is unchanged. The
  `rows[ix].stage == meta.name` alignment against `ordered_rows`'s
  configured-first ordering holds by construction (same `stage_name()` filter).
- **§D6 (Enter opens the worktree)** — `pipeline_landing` is pure, `Row` first
  with the original predicate, `Open` from `sidebar_db_worktrees`, idempotent
  on group name, `None` keeps the notice. Loop arm mirrors `creating::open_tab`.
- **§D7 (help door)** — `MonitorOutcome::Help` (not Passthrough) with the
  rationale documented; `?`/F1 sit above the per-tab letters and below the
  filter/confirm early-returns; `help::open_at(..., MONITOR)`; help renders
  after the monitor (`run.rs:12066`) and owns keys before the monitor block
  (`run.rs:13520`) — verified. `overlay:monitor` in `context::vocabulary()`,
  claimed by the page, all six help ratchet tests green.

**§3 invariants:** no new timer/thread/channel (StageMeta rides the existing
off-loop sample; the hydration kick I added reuses the coalesced
`model_refresh_pending` door — no new wake source); `render_plan` untouched;
no glyph/color literal at any new draw site; no new ignored `Result`; help
ratchets untouched and green; core edit is pure with three unit tests.

## 2. The done-report "Unverified" items — disposition

### Chunk 1

| Item                                               | Disposition                                                                                                                                                                                                                                                                                               |
| -------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| No full-workspace gate                             | Per policy, Lead's pre-PR run. Reviewer ran the scoped equivalents instead (see §4).                                                                                                                                                                                                                      |
| e2e not run                                        | **Agreed, by design** (§4 of the design): every chunk changes frames; baselines were stale before this lane. Follow-up for whoever revives the gate.                                                                                                                                                      |
| Whole-crate host tests not run                     | Reviewer ran 380 scoped host tests (monitor/sections/help/detail/chrome/model_eq/hydrate) — all green, covering every `detail*` construction site that `sel: None` touched.                                                                                                                               |
| `table_cols` callers unmeasured                    | **Verified**: the only external caller is `detail/calendar/render.rs:204`; every table it sizes is `sel: None` (lines 187/337/422), so it is numerically unchanged. The four `sel: Some` tables are monitor-internal and sized by `Self::dims`.                                                           |
| Pre-existing `⏸` literal in `tab_bar`'s paused run | **Confirmed pre-existing** (`git show main` line 1335) and **confirmed out of reach**: no `Pause` token exists in `termcaps::Glyph`, and the ratchet's U+2500–259F window cannot see U+23F8. Fixing it means minting a core token, which §D4 deliberately declined. Debt note, not a defect of this lane. |

### Chunk 2

| Item                                       | Disposition                                                                                                                                                    |
| ------------------------------------------ | -------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| thegn-core 95% coverage unmeasured         | The core edit is `glyph_token` + tests; the coverage gate is CI-only. Reviewer ran the `issue::` tests green; risk accepted for the Lead's `just coverage`.    |
| chrome_tests construct `FrameModel` widely | Ran `chrome::` scoped — green (`procs_disabled` has a safe `Default` of false = enabled, matching `MonitorConfig::default()`, and `hydration_eq` includes it). |
| Chevron not eyeballed live                 | Its ASCII fallback (`>`) comes from the shared ladder and `board_row_glyphs_degrade_to_ascii` pins the degradation. Accepted.                                  |
| treefmt stray reformat reverted            | `git status` clean at those commits; confirmed.                                                                                                                |

### Chunk 3

| Item                                   | Disposition                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                     |
| -------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `?` with SHIFT modifier                | **Verified by reading the handler**: only `ALT\|SUPER` early-return and the `CTRL` arm precede the match; SHIFT falls through to the `Char('?')` arm.                                                                                                                                                                                                                                                                                                                                                                           |
| `Open` branch not exercised end-to-end | The reasoning was sound (missing-leaf lazy materialize; loop tail identical to `open_tab`) — and reviewing it surfaced a real gap, now fixed: the arm never kicked model hydration, so a board-opened worktree waited for the next periodic tick before its git/diff data arrived while the resident `Row` arm kicked immediately. **Fixed in `bfdea3bd`.** Residual: a one-time visual confirmation in a live session, which only a human at the keyboard can do; the pure resolution and the loop tail are both now verified. |
| Help page placement not eyeballed      | `registry_validates_cleanly` + the context/action ratchets are green; ordering rule (order ties break by title) read in `help/registry.rs`. Accepted.                                                                                                                                                                                                                                                                                                                                                                           |
| Containers help wording stale          | **Fixed by reviewer** in `06f63ed2` — the section described the pre-chunk-2 header ("sums thegn's footprint") instead of the `containers` heading with the owned/foreign split leading the note.                                                                                                                                                                                                                                                                                                                                |

## 3. Corrections applied by the reviewer

| sha        | subject                                                                                                     |
| ---------- | ----------------------------------------------------------------------------------------------------------- |
| `55e9f0ac` | `merge(main): THE-70/THE-83 into THE-75 monitor-fixes; keep both help paragraphs` (the required first step) |
| `bfdea3bd` | `fix(monitor): kick model hydration when a board Enter opens a non-resident worktree (THE-75)`              |
| `06f63ed2` | `docs(help): the containers header note now leads with the owned/foreign split (THE-75)`                    |

Both fixes re-verified: `just quick thegn-host` clean, help ratchets green.

## 4. Verification run by the reviewer

- `just quick thegn-host`, `just quick thegn-core` — clean (clippy `-D warnings`).
- `cargo nextest run -p thegn-host monitor:: sections:: help:: detail:: chrome:: model_eq:: hydrate::` — **380 passed, 0 failed**.
- `cargo nextest run -p thegn-host ratchet` (glyph/color/platform/caret ratchets) and `-p thegn-host help::ratchet` — all green.
- `cargo nextest run -p thegn-core issue:: ratchet` — green.

## 5. Flags — no action required for this lane

1. **e2e baselines are stale branch-wide** (45 snapshots), as the design's §4
   anticipated and each done report repeated. Whoever revives `just e2e`
   re-records with `just e2e-update` on both platforms; until then this gate
   checks nothing here.
2. **Pre-existing, out of scope per §6:** a configured stage literally named
   `unstaged` draws twice on the board (idle heading at its config position,
   then the trailing `UNSTAGED` group — rows still appear exactly once), and
   `ordered_rows` can push `UNSTAGED` into its order twice under that config.
   Pathological config; pre-dates this branch.
3. **`⏸ paused` glyph bypasses the ladder** (`monitor.rs`, pre-existing). Needs
   a `Glyph::Pause` token in core to fix; §D4 declined core growth for it.
   Candidate for a future ratchet widening (the current window cannot see
   U+23F8).
4. **Full gates remain outstanding by policy** (`just test`, `just coverage`,
   `just ci` — the Lead's pre-PR run; CI is dispatch-only). The scoped runs in
   §4 cover every touched area but are not a substitute for the gate.

## 6. Verdict

**APPROVED** — ready for the Lead's pre-push gate (`just test`, smoke, then
`just ci` when landing). No revision chunk issued; nothing was found that the
two applied corrections plus the recorded flags do not cover.
