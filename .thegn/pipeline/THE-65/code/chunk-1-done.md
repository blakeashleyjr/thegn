# Chunk 1 done — `thegn_core::usage_view`, the pure layout model

Branch: `tg/the-65-usage-panel` · Commit: `35184124`
`feat(usage): a pure worst-first layout model for the usage surfaces (THE-65)`

## What was implemented

- **`crates/thegn-core/src/usage_view.rs` (new)** — the pure, substrate-free
  layout module per the chunk spec and design §3.1:
  - Types exactly as specced: `ViewOpts`, `MetricRow`, `AccountView`,
    `UsageView` (no extra fields, doc comments carried over).
  - `order(&[AccountUsage]) -> Vec<usize>` — worst-first by
    `(state_rank, peak_percent desc, original_index)`; ranks Ok-with-windows 0,
    Ok-without-windows 1, Loading 2, Unavailable 3; stable sort with index
    tiebreak; `f32::total_cmp` (no `partial_cmp().unwrap()`); returns indices,
    input untouched.
  - `metric_name(&UsageWindow) -> String` — minutes first (`<60` minute,
    `<1440` hour, else day; whole numbers only, non-integral falls back to the
    label verbatim), model qualifier preserved from a leading base token
    (`session`/`weekly`/`5h`/`7d`), no-length labels pass through untouched.
  - `build(&[AccountUsage], &BTreeMap<String, Vec<(i64, f32)>>, &ViewOpts)`
    — peak-window selection via `AccountUsage::peak_window()` (never
    `windows.first()`), one `name_w` measured with
    `unicode_width::UnicodeWidthStr` across every selected window of every
    account, explicit-space padding in display cells (no `{:<n$}`),
    `usage::tone_at` with the **caller's** thresholds (§1.6 fix), resets
    phrases (`resets in …` / `resets now` / empty), forecast phrases via
    `usage::forecast_exhaustion` + `fmt_resets_in` (`runs out in …`, empty on
    `None`), `history_key` = `format!("{account_key}#{window_label}")`
    byte-identical to `usage_dash::history_key`, one-line facts
    (`org · seat · tier · <last-two-home-components>`, whitespace-only fields
    skipped, no `$HOME`), notes (`plan` / `unavailable: …` / `…` /
    `no windows reported`), accounts emitted worst-first via `order`.
  - `legend() -> &'static [&'static str]` — unjoined parts, no separator glyph.
- **`crates/thegn-core/src/lib.rs`** — one `pub mod usage_view;` line in
  alphabetical position (after `usage_tokens`, before `util`).
- 15 in-file tests covering all 10 required cases plus extras (notes variants,
  non-Ok rows/tone, `runs out now`, wide-glyph padding, zero-length minutes,
  legend).

## Deviation from the spec (flagged for review)

**`crates/thegn-core/Cargo.toml` was touched — the spec said it wouldn't be.**
The chunk mandates `use unicode_width::UnicodeWidthStr`, and the design (§3.1)
cites `unicode-width` as an existing thegn-core dependency — but it was
actually only under `[dev-dependencies]` (`Cargo.toml:95`, for the glyph-table
tests). The module cannot compile without it as a real dependency, and
hand-rolling width tables would violate the explicit instruction. Resolution:
**promoted the existing `unicode-width = "0.2"` declaration from
`[dev-dependencies]` to `[dependencies]`** — same crate, same version, already
in the lock graph (no `Cargo.lock` change), and the same regular dependency
thegn-host already declares (`crates/thegn-host/Cargo.toml:126`). No new
external crate enters the tree; the "thegn-core gains no dependency" intent
(no substrate) is preserved. Reviewers may prefer to revert this and take a
different route, but there is no spec-compliant alternative that compiles.

## Verification (scoped per dev-loop policy)

- `just quick thegn-core` — **green** (clippy `-D warnings`, lib).
- `cargo nextest run -p thegn-core usage_view` — **15/15 pass**.
- `git status` after commit — only the three files above; **no** `thegn-host`
  file, no `openspec/` file, no ratchet file touched.
- rustfmt/treefmt clean (pre-commit hook passed).

## Unverified

- **Coverage percentage not measured.** `usage_view.rs` is under the 95%-line
  gate but `just coverage` is a forbidden heavy gate; every branch I could
  enumerate has a test (including the `history.get` miss path, the
  non-integral-minutes fallback, one-component and root homes, plan-less Ok
  note), but the actual percentage is unmeasured — the review/CI stage should
  confirm.
- **Clippy on test targets not run.** `just quick` checks lib/bin only;
  `cargo nextest run` compiled the test target (so it builds), but
  `clippy --all-targets` on it was not run to avoid an extra compile. Any
  test-only lint would surface at pre-push.
- **Downstream compile not checked.** `thegn-host` (chunks 2–3) does not yet
  consume this module; I did not compile the host. Nothing in the host was
  touched, so it still compiles as before, but the two consumers'
  integration is untested by definition at this stage.
- **No e2e** (per instructions; design §4 confirms no usage frame is
  snapshotted, so none is needed).
