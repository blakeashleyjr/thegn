# THE-65 — Agent usage panel is hard to grep: a scannable layout

Architect design. Linear: <https://linear.app/blakeashley/issue/THE-65>
Branch: `tg/the-65-usage-panel`.

## 1. What is actually wrong (evidence, not hypothesis)

The usage data has three renderers and two of them are dense by construction.
Everything below is read off the current tree.

### 1.1 The overlay emits ~4 blocks per account with no separator

`usage_sections` (`crates/thegn-host/src/detail/usage_dash.rs:338-369`) pushes,
per account and unconditionally:

- `account_heading` (line 348) — 1 row;
- `account_facts` (line 349) — a `Section::Grid { cols: 2 }` of up to 5 pairs,
  i.e. **up to 3 rows** (`sections.rs:198`: `cells.len().div_ceil(cols)`);
- a `Section::Table` of **every** window (line 355-358) — N rows;
- a `trend_row` **per window** (line 359-363) — up to N more rows.

So one account costs `1 + 3 + 2N` rows. At the observed shape (8 accounts, 2-3
windows) that is ~40 rows in an 88×30 box, and there is **no blank line between
accounts** — `sections::spacer()` exists (`sections.rs:439`) and `usage_dash`
never calls it. The blocks run together, which is precisely "hard to grep".

### 1.2 Half of those rows say nothing

`trend_row` (`usage_dash.rs:244-275`) draws a sparkline whenever the history has
two points in the current run. The _forecast_ — the only actionable half — is
computed at line 263 and is `String::new()` whenever `forecast_exhaustion`
returns `None`, which is the common case (`usage.rs:399-403`: a window that
resets before it exhausts has no forecast worth showing). The result is a second
full-width row per window carrying a decorative squiggle and an empty value.

### 1.3 The columns do not line up between accounts

`draw_table` sizes columns from **its own** `TableSection`
(`sections.rs:526-544`, called at `sections.rs:558`). `usage_sections` emits
**one table per account** (`usage_dash.rs:355`). Account A's windows are
`session`/`weekly` (`usage.rs:818`), account B's are `5h`/`7d`/`7d opus`
(`usage.rs:981-986`), and Claude's `limits[]` path can produce `weekly Fable`
(`usage.rs:903-924`). Different widest-label ⇒ **different bar start column and
different `%` column per account.** Nothing lines up down the screen, so the
eye cannot scan the percentages as a column.

### 1.4 The dense identity facts sit _above_ the numbers

`account_facts` renders before the window table (`usage_dash.rs:349` precedes
`:355`), and the widest fact is `home` — a path like
`~/.claude-profiles/regclaude2/.claude`, which the module's own comment
(`usage_dash.rs:58-62`) says is most of the 88-cell box. The reader must scroll
past the least-scannable data to reach the number they opened the overlay for.

### 1.5 Labels are provider jargon, not language

`w.label` is whatever the provider said: `session`, `weekly`, `5h`, `7d`,
`7d opus`, `weekly Fable`, `window 1` (`usage.rs:818`, `:981-986`, `:903-924`,
`:1197-1199`). The window length is available as `window_minutes` /
`len_label()` (`usage.rs:51-87`) and is rendered as a ghost `/5h` fragment
(`usage_dash.rs:234-237`) — a fact the reader has to assemble.

### 1.6 The overlay ignores the configured thresholds (correctness bug)

`usage_dash::tone_tok` (`:144-150`) calls `thegn_core::usage::tone`, which is
hard-wired to `DEFAULT_WARN_PERCENT` / `DEFAULT_CRIT_PERCENT`
(`usage.rs:276-283`). The panel section calls `tone_at` with
`ctx.model.usage_cfg.warn_percent / crit_percent`
(`panel/sections/usage.rs:27-34`), and so does the statusbar badge
(`statusbar_badges.rs:263`). With `[usage] warn_percent = 60` a window is amber
in the panel and green in the overlay. **The same number must not have two
colours.** This change fixes it while it is rewiring the tone path anyway.

### 1.7 The bars do not degrade

`Cell::Bar` draws through `viz::bar_track` (`sections.rs:576-580`), and
`viz::bar_track` → `viz::hbar` hard-code `█`, the eighth-block ladder and `░`
(`crates/thegn-core/src/viz.rs:59-76`). The panel's `bar_segs`
(`panel/sections/mod.rs:121-124`) does the same. `thegn-core` is outside the
host glyph-literal ratchet (`test/glyph-literal-ratchet.txt` header;
`platform_ratchet_tests.rs:72-82` scans `crates/thegn-host/src` only), so this
never tripped a gate — but on `[theme] glyphs = ascii` / a non-UTF-8 locale the
gauges render as mojibake. `GlyphSet` already carries `bar_fill: "="` /
`bar_empty: "-"` for exactly this (`termcaps.rs:417-418`, ASCII table at
`:544-545`), and `loading/plan.rs:89-96` shows the established pattern.

### 1.8 A legend has nowhere to live today

`DetailOverlay::hint` is only drawn on the `List` branch (`detail.rs:918-921`
→ `render_list(..., self.hint)`); the `Sections` branch (`detail.rs:925-927`)
never reads it. A footer legend for this overlay must therefore be a
**`Section`**, not a `hint`.

### 1.9 Everything the vocabulary draws as a "heading" is dim

`draw_section` renders both `Heading` and `HeadingToned` labels with
`Tok::Slot(S::Dim)` (`sections.rs:373-389`); only the _note_ carries tone. So
there is no way to make an account name outrank a metric row — the "visual
hierarchy (headers)" the issue asks for is not expressible in the current
`Section` enum.

## 2. Scope

In scope (the issue body): grouping, one aligned line per metric, plain-language
labels, visual hierarchy, one bar per limit, a footer legend, pure render, caps
degradation, help-page updates, unit tests on the layout function. Plus the
tone-threshold bug (§1.6), which the rewiring exposes and which would otherwise
ship a _newly_ aligned column of wrong colours.

**Explicitly NOT in scope**, and left for a later slice of the existing openspec
change `openspec/changes/make-usage-overlay-scannable/`:

- its item 2, **expand/collapse a selected account** — that needs a new action
  id, a keymap binding, `run.rs` wiring and three help ratchets. This design
  reaches the same density win with no new keys (§3.4), so the interaction is a
  genuine follow-up rather than a prerequisite.
- its item 4, **`thegn usage --json`** (the `usage.snapshot` capability row) —
  an unrelated surface with its own catalog ratchet.

Coders **must not** edit anything under `openspec/` in this change: those files
are validated `--strict` in `just ci` and a partial tick-off would misreport the
change as delivered.

Out of scope and untouched: the statusbar gauge (`statusbar_badges.rs`), the
gather/poll path (`actions::spawn_usage`, `thegn_svc::usage`), the DB history
schema, `[usage]` config keys (none added).

## 3. The design

### 3.1 A pure layout model in `thegn-core`

New module `crates/thegn-core/src/usage_view.rs`. `thegn-core` already owns the
usage _formatting_ vocabulary — `fmt_resets_in`, `fmt_tokens`, `tone_at`,
`used_frac`, `peak_across`, `short_label` (`usage.rs:281-464`) — so the layout
decision belongs beside them, not duplicated in two host renderers. It is
substrate-free (plain data in, `String`s and enums out; `unicode-width` is
already a `thegn-core` dependency, `crates/thegn-core/Cargo.toml:95`) and it
lands under the 95%-line coverage gate, which is where the issue's "unit tests
for the layout/format function" requirement is satisfied.

It answers five questions, each independently testable:

1. **Order** — `order(accounts) -> Vec<usize>`: worst-first by peak percent,
   with `Ok`-with-windows, then `Ok`-without-windows, then `Loading`, then
   `Unavailable`; ties keep discovery order (stable, so the list does not flip
   between polls — the same reasoning as `peak_across`, `usage.rs:309-310`).
   It returns **indices**; callers reorder a view, never `model.usage` itself
   (see §5).
2. **Name** — `metric_name(w) -> String`: plain language built from
   `window_minutes` first and the provider label second, so `5h`/`session` +
   300 both read `5-hour window`, `7d`/`weekly` + 10080 read `7-day window`,
   and a model-scoped cap keeps its qualifier: `7-day window (opus)`,
   `7-day window (Fable)`. With no stated length, the provider label passes
   through verbatim (`window 1`), never inventing a duration.
3. **Alignment** — one `name_w` computed across **every** row of **every**
   account, and names emitted already padded to it. That makes the per-table
   sizing in `sections.rs:526` a no-op and gets a single bar/percent column down
   the whole overlay without touching the drawing code.
4. **Rows** — one `MetricRow` per limit: padded name, `frac`, a stable-width
   `pct` string, `UsageTone` (from the **caller's** thresholds), the
   `resets in …` phrase, an optional `runs out in …` forecast phrase, and the
   `history_key` so the host can find the series without recomputing the key
   format.
5. **Legend** — `legend() -> &'static [&'static str]`: the parts, unjoined. The
   host joins them with `caps::glyph(Glyph::Middot)`, so the separator degrades
   like everything else.

Sketch (exact names are the coder's to finalise, the shape is not):

```rust
pub struct ViewOpts { pub now: i64, pub warn_percent: f32, pub crit_percent: f32, pub peak_only: bool }
pub struct MetricRow { pub name: String, pub pct: String, pub used_percent: f32,
                       pub frac: f32, pub tone: UsageTone, pub resets: String,
                       pub forecast: String, pub history_key: String }
pub struct AccountView { pub key: String, pub label: String, pub note: String,
                         pub state: UsageState, pub tone: Option<UsageTone>,
                         pub facts: String, pub rows: Vec<MetricRow> }
pub struct UsageView { pub accounts: Vec<AccountView>, pub name_w: usize, pub summary: String }

pub fn build(accounts: &[AccountUsage],
             history: &BTreeMap<String, Vec<(i64, f32)>>,
             opts: &ViewOpts) -> UsageView;
```

`AccountView::tone` is `Some(tone_at(peak, warn, crit))` for a readable account
and `None` otherwise (the caller draws those dim) — core states severity, the
host maps it to a `Tok`, so no colour literal moves out of the chokepoint.

`facts` is **one line**, not a grid: `org · seat · tier · <home>`, with the
credential home abbreviated to its **last two path components**
(`regclaude2/.claude`). That keeps §1.4's discriminator — which is the whole
reason the home is shown (`usage.rs:129-131`) — at a tenth of the width, with no
`$HOME` lookup and therefore no I/O in core.

### 3.2 The overlay (`detail/usage_dash.rs`)

Per account, in order:

```
usage                                          8 accounts · worst first
blake@example.com (Acme)                                  max
  7-day window        ███████████████░  94%   resets in 2d 3h
  5-hour window       ██░░░░░░░░░░░░░░  12%   resets in 41m
  org Acme · seat team_standard · regclaude2/.claude
                                                         (blank)
…
local tokens — host-wide, not per account            412 responses
…
bar = share of the limit used · % = used now · worst first
```

- Account heading is `HeadingToned` with the **peak window's tone on the label**
  and the plan (or `unavailable: …`) as the note — the scan question is answered
  by reading order and colour alone.
- Metric rows are one `Section::Table` per account, names pre-padded to
  `name_w`; the bar keeps `BAR_W = 16` (`usage_dash.rs:24`).
- The forecast folds into the metric row's tail as `runs out in 3h 12m`; the
  standalone `Sparkrow` is emitted **only where a forecast exists**, which is
  the fix for §1.2 and keeps the actionable half of the trend.
- Facts move **below** the numbers, as one dim row (§1.4/§3.1).
- `sections::spacer()` between accounts (§1.1).
- A trailing dim `Section::Heading` carries the legend (§1.8).
- The token rollup block keeps its current shape and its "host-wide" heading —
  that disclaimer is load-bearing (`usage_dash.rs:277-294`, and the help page
  says so at `docs/help/ai-usage.md:119-127`). It stays _after_ the accounts and
  _before_ the legend.

`usage_overlay` / `apply_usage` grow one parameter carrying the configured
thresholds (§1.6). Five call sites, all of which already have `model` in scope:
`detail.rs:2097`, `run.rs:10544`, `run.rs:10598`, `run.rs:17000`,
`run.rs:19364`. These are parameter-only edits — **no new logic in `run.rs`**,
per the god-file rule.

### 3.3 Visual hierarchy needs one vocabulary change

`Section::HeadingToned` gains `label_tone: Tok` and draws its label with that
token, bold (`sections.rs:383-389`; `Sparkrow` already bolds its value at
`:429`, so the attribute is established). Existing constructions pass
`Tok::Slot(S::Dim)` and are byte-identical on screen. Only two files construct
it — `detail/usage_dash.rs` and `detail/status_modal.rs`.

This is the minimum change that makes §1.9 expressible; every other route (a
one-row `Table`, a `KeyVal`) also draws its key dim.

### 3.4 Why no expand key

Density is recovered by deleting rows that said nothing (§1.2), collapsing the
facts grid to a line (§3.1) and moving it below the numbers — from `1 + 3 + 2N`
rows per account to `1 + N + 1 + 1`. At 8 accounts × 2 windows that is 40 rows →
~32 with a blank between each, and the first line of every account is now the
answer. An expand/collapse toggle costs a new `ACTION_SPECS` id, a keymap
binding, `run.rs` key routing and three help ratchets for a second-order win;
it is the right follow-up, not this change (§2).

### 3.5 The panel section (`panel/sections/usage.rs`)

Same `usage_view::build`, projected into `PanelRow`s. The three width tiers
survive because they map onto the model:

- **Normal** — `ViewOpts { peak_only: true }`: one metric row per account.
- **Half** — every metric row, indented, plain-language names, aligned.
- **Full** — the above plus the facts line, the absolute reset, the token block
  and the legend.

The ordering, names, tones and column width now come from one function, so a
window that reads `7-day window 94%` in the panel reads identically in the
overlay. This is the shared-list-fn lesson from the panel audits, applied.

### 3.6 Degrading the gauges

Add `crate::caps::bar_track(frac, w) -> (String, String)` in `caps.rs` — the
glyph chokepoint, and the one file the glyph ratchet exempts
(`platform_ratchet_tests.rs:76`). On `UnicodeLevel::Full | Basic` it delegates
to `viz::bar_track` **verbatim** (byte-identical output, so no snapshot moves);
on `Ascii` it fills `g.bar_fill` / `g.bar_empty`
(`termcaps.rs:417-418`, `:544-545`) exactly as `loading/plan.rs:89-96` does.

Route the two shared draw sites through it: the `Cell::Bar` arm of `draw_table`
(`sections.rs:576-580`) and `bar_segs` (`panel/sections/mod.rs:121-124`). Fixing
it at the chokepoints rather than in the usage renderer is what stops the next
gauge from re-introducing the bug — and it is invisible on any UTF-8 terminal.

## 4. Invariants this change is measured against

- **Pure render, no new wake source.** Nothing here spawns, polls, sleeps or
  reads a file. `usage_view::build` is a pure function of the payload the
  refresh channel already delivers; the overlay still re-renders only on the two
  existing `RefreshKind` arms (`run.rs:10544`, `:10598`).
- **Render decision untouched.** No change to `render_plan::plan`; overlay
  content damage is `Full` today and stays `Full`.
- **Degrade at the edges.** No colour literal (core states `UsageTone`, the host
  maps to `Tok`); no glyph literal (bars through `caps`, the legend separator
  through `Glyph::Middot`). No new entry in `test/glyph-literal-ratchet.txt` or
  `test/color-literal-ratchet.txt` — a coder who needs one has made a mistake.
- **`thegn-core` stays substrate-free.** `usage_view` takes data and returns
  data. No tokio, no I/O, no `$HOME`.
- **Coverage.** `usage_view.rs` is not in the justfile's `cov_ignore`
  (`justfile:514`), so it is gated at 95% lines. Test every branch.
- **Help ratchets.** No new action id, no new panel context ⇒ no ratchet file
  changes. `docs/help/ai-usage.md` already claims `open-usage` and
  `contexts: [panel:usage]` (`:6-7`); it must be updated so its prose still
  describes what the surfaces actually show (help-prose ratchet).
- **e2e.** `grep -rl usage test/muse/snapshots/` is empty across all 17
  baselines — no usage frame is snapshotted, so **no re-record is needed and no
  coder should run `just e2e`.**

## 5. Traps

1. **Never sort `model.usage` in place.** Its order is load-bearing elsewhere:
   `peak_across` returns an index into it (`usage.rs:311-323`) for the statusbar
   badge, and the alert handler keys off the same slice. `order()` returns
   indices; build a view.
2. **The history key format is shared.** `usage_dash::history_key`
   (`:54-56`) is also called by the panel (`panel/sections/usage.rs:242`) and by
   the sampler that writes the rows. If `usage_view` emits `history_key`, it
   must produce the identical `"{key}#{label}"` string — off-by-one there
   silently kills every forecast, and nothing fails loudly.
3. **`Section::height` must agree with what is drawn** (`sections.rs:14-17`,
   `:192-201`). Adding a spacer or a legend row without its height is how the
   tail of a scrolled stack becomes unreachable. `spacer()` is a `Heading`, so
   it is already height-1 — use it, don't invent a variant.
4. **`hint` is not a footer here** (§1.8). A legend set on `ov.hint` will simply
   not render, and the test that "passes" will be asserting on the struct field.
5. **Plain-language names must not invent a duration.** `window_minutes` is
   `None` for Antigravity (`usage.rs:1203-1207`) and for Claude's flat fallback
   when the group is unknown (`usage.rs:928-937`). No length ⇒ pass the label
   through. `len_label()` already refuses to render a zero (`usage.rs:69-81`);
   don't undo that.
6. **`peak_only` is not `windows.first()`.** Use `peak_window()`
   (`usage.rs:256-263`) — a 7-day window at 91% must not hide behind a
   freshly-reset 5-hour one at 2%, which the panel already has a test for
   (`panel/sections/usage.rs:296-308`).
7. **Padding is display cells, not chars.** Core has `unicode-width`; use it
   (`sections.rs:504-507` shows the same care in `draw_grid`, and its comment
   says why `{:<n$}` drifts).
8. **The ASCII bar must keep `bar.len() + track.len() == w`**, the invariant
   `viz::bar_track` documents (`viz.rs:71-76`) and `sections.rs:576-580` relies
   on for column widths.
