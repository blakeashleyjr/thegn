# Chunk 1 — `thegn_core::usage_view`, the pure layout model

Read `.thegn/pipeline/THE-65/architect/design.md` first (§3.1 and §5 especially).
Work in `/home/blake/.superzej/worktrees/thegn/tg-the-65-usage-panel`.

## Dependency / overlap

- **Runs first, alone.** Chunks 2 and 3 both `use thegn_core::usage_view` and
  cannot compile until this is committed.
- Files are disjoint from both other chunks (they touch only `thegn-host` and
  `docs/`).
- Do **not** touch any `thegn-host` file, and do **not** edit anything under
  `openspec/`.

## Files touched (exact)

- `crates/thegn-core/src/usage_view.rs` — **new**
- `crates/thegn-core/src/lib.rs` — one `pub mod usage_view;` line, in
  alphabetical position beside `pub mod usage;`

Nothing else. `crates/thegn-core/src/usage.rs` stays as it is — this module
consumes it.

## Approach

A pure module: plain data in, `String`s and enums out. No I/O, no `$HOME`, no
tokio, no `std::time::SystemTime` (`now` is a parameter). This is the single
source of ordering, naming, tone and column width for **both** host usage
renderers, so anything either surface would otherwise decide for itself belongs
here.

### Types

```rust
pub struct ViewOpts {
    /// Epoch seconds; the caller passes `thegn_core::util::now()`.
    pub now: i64,
    /// `[usage] warn_percent` / `crit_percent` — NOT the module defaults.
    pub warn_percent: f32,
    pub crit_percent: f32,
    /// Only the peak window per account (the panel's resting width).
    pub peak_only: bool,
}

pub struct MetricRow {
    /// Plain-language name, ALREADY padded to `UsageView::name_w` display cells.
    pub name: String,
    /// Right-aligned used percent, stable width: `" 94%"`, `"100%"`, `"  2%"`.
    pub pct: String,
    pub used_percent: f32,
    /// `usage::used_frac(used_percent)` — the bar fill.
    pub frac: f32,
    pub tone: UsageTone,
    /// `"resets in 2h 14m"`, or empty when the provider stated no reset.
    pub resets: String,
    /// `"runs out in 3h 12m"` when `forecast_exhaustion` yields one, else empty.
    pub forecast: String,
    /// `"{account key}#{window label}"` — the history-map key.
    pub history_key: String,
}

pub struct AccountView {
    pub key: String,
    pub label: String,
    /// The plan (`"max"`), or `"unavailable: token expired"`, or `"…"` while
    /// loading, or `"no windows reported"`. Never empty for a non-Ok account.
    pub note: String,
    pub state: UsageState,
    /// Peak window's tone; `None` when there is nothing to tone (caller draws
    /// those dim). Core states severity, the host picks the colour.
    pub tone: Option<UsageTone>,
    pub peak_percent: Option<f32>,
    /// One line: `org Acme · seat team_standard · regclaude2/.claude`.
    /// Empty when nothing is known.
    pub facts: String,
    pub rows: Vec<MetricRow>,
}

pub struct UsageView {
    /// Worst-first (see `order`).
    pub accounts: Vec<AccountView>,
    /// The one name-column width every row was padded to.
    pub name_w: usize,
    /// `"8 accounts"` / `"1 account"`.
    pub summary: String,
}
```

### Functions

- `pub fn order(accounts: &[AccountUsage]) -> Vec<usize>` — worst-first.
  Sort key: `(state_rank, Reverse(peak_percent), original_index)` where
  `state_rank` is `Ok`-with-windows `0`, `Ok`-without-windows `1`, `Loading` `2`,
  `Unavailable` `3`. Use a **stable** sort with the index as the final tiebreak
  so equal accounts keep discovery order and the list does not flip between
  polls (same reasoning as `usage::peak_across`, `usage.rs:309-310`).
  Compare `f32` with `total_cmp` — do not `unwrap` a `partial_cmp`.
  **Returns indices. It must not clone, reorder or otherwise touch the input.**

- `pub fn metric_name(w: &UsageWindow) -> String` — plain language, from
  `window_minutes` first and the label second:
  - length present → `"5-hour window"`, `"7-day window"`, `"45-minute window"`
    (derive the unit from the minutes: `<60` minute, `<1440` hour, else day;
    render a whole number, never `1.5-hour`; fall through to the label when the
    minutes do not divide evenly);
  - a model-scoped qualifier in the label is preserved in parentheses:
    `"7d opus"` → `"7-day window (opus)"`, `"weekly Fable"` →
    `"7-day window (Fable)"`. The qualifier is whatever remains after stripping
    the leading base token (`session`, `weekly`, `5h`, `7d`);
  - **no stated length → the provider's label passes through verbatim**
    (`"window 1"`, `"limit"`). Never invent a duration. See design §5.5.

- `pub fn build(accounts: &[AccountUsage], history: &BTreeMap<String, Vec<(i64, f32)>>, opts: &ViewOpts) -> UsageView`
  - selects windows: all, or `peak_window()` only when `opts.peak_only` — use
    `AccountUsage::peak_window()` (`usage.rs:256-263`), **never**
    `windows.first()` (design §5.6);
  - names every selected window, measures the widest in **display cells**
    (`unicode_width::UnicodeWidthStr` — already a `thegn-core` dep,
    `Cargo.toml:95`), then pads every name to that one width. Pad with explicit
    spaces computed from the measured width, not `format!("{:<n$}")` — that
    counts chars and drifts on wide glyphs (`sections.rs:504-507` says why);
  - tones with `usage::tone_at(pct, opts.warn_percent, opts.crit_percent)`;
  - `resets` via `usage::fmt_resets_in(w.resets_at, opts.now)`, prefixed
    `"resets in "` — except the `"now"` case, which must read `"resets now"`
    rather than `"resets in now"` (`fmt_resets_in` returns `"now"` once elapsed,
    `usage.rs:331-333`);
  - `forecast` via `usage::forecast_exhaustion(hist, now, w.resets_at)` then
    `fmt_resets_in`, prefixed `"runs out in "`; empty when either returns `None`;
  - `history_key` **must** be byte-identical to today's
    `format!("{account_key}#{window_label}")`
    (`crates/thegn-host/src/detail/usage_dash.rs:54-56`) — a mismatch silently
    kills every forecast and sparkline and nothing fails loudly (design §5.2);
  - `facts`: `org <v>`, `seat <v>`, `tier <v>`, then the credential home
    abbreviated to its **last two path components** (`regclaude2/.claude`),
    joined with `" · "`. Skip any field that is absent or whitespace-only —
    a bare account gets an empty string, never a row of "unknown" (the existing
    behaviour, `usage_dash.rs:191-197`). A home with fewer than two components
    renders whatever it has. Do not read `$HOME`.

- `pub fn legend() -> &'static [&'static str]` — the parts, **unjoined**, e.g.
  `["bar = share of the limit used", "% = used now", "worst first"]`. The host
  joins them with the caps middot, so no separator glyph is baked in here.

## Tests (in-file `#[cfg(test)] mod tests`)

`usage_view.rs` is not in the justfile's `cov_ignore` (`justfile:514`), so it is
under the **95%-line coverage gate**. Cover every branch. At minimum:

1. `order` — worst-first across three Ok accounts; `Loading` and `Unavailable`
   sink below every Ok account; an Ok account with no windows sits between them;
   two accounts at the same percent keep discovery order; the input slice is
   unmodified.
2. `metric_name` — `("session", 300)` and `("5h", 300)` both → `5-hour window`;
   `("weekly", 10080)` and `("7d", 10080)` → `7-day window`;
   `("7d opus", 10080)` → `7-day window (opus)`; `("weekly Fable", 10080)` →
   `7-day window (Fable)`; `("window 1", None)` → `window 1` verbatim;
   a sub-hour length reads in minutes.
3. `build` alignment — two accounts whose widest names differ produce rows padded
   to **one** width; assert the padded display width, not the char count.
4. `build` tone — the same percent tones differently under
   `warn_percent = 60` and under the defaults (this is the §1.6 bug; pin it).
5. `build` peak_only — an account with `5h` at 2% and `7d` at 91% yields exactly
   one row, and it is the 7-day one.
6. `build` resets — a known deadline reads `resets in 1h 0m`; an elapsed one
   reads `resets now`; an unknown one is empty.
7. `build` forecast — a climbing run produces `runs out in …`; a flat run, a run
   shorter than `MIN_FORECAST_SPAN_SECS`, and a window that resets first all
   produce an empty string.
8. `history_key` round-trip — the key `build` emits finds the series the caller
   inserted with `format!("{key}#{label}")`.
9. `facts` — nothing known → empty; a rich account → `org`, `seat`, `tier` and
   the two-component home, in that order; a whitespace-only field is skipped.
10. `summary` — singular `1 account`, plural otherwise; empty input yields an
    empty `accounts` vec and does not panic.

## Commands to run (scoped — nothing full-workspace)

```sh
just quick thegn-core
cargo nextest run -p thegn-core usage_view
```

Do **not** run `just test`, `just ci`, `just coverage` or `just e2e`.

## Done criteria

- `crates/thegn-core/src/usage_view.rs` exists, is `pub mod`-declared, and both
  commands above are green.
- No `thegn-host` file, no `openspec/` file, no ratchet file changed.
- `thegn-core` gains no dependency.
- Committed on `tg/the-65-usage-panel` with **exactly** this subject:

```
feat(usage): a pure worst-first layout model for the usage surfaces (THE-65)
```
