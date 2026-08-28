# THE-65 architect review — verdict

Branch `tg/the-65-usage-panel`, reviewed at `18b84e2c`
(after the mandated `git merge main` → `2cb6ea54`, and one review-fix commit).

**Verdict: APPROVED.** No revision chunk. One review commit was applied
(`fix(usage): review — clippy -D warnings on the new test targets (THE-65)`).

## What the branch is

Chunk 1 (`35184124`): `thegn_core::usage_view` — the pure layout model
(ordering / plain-language naming / tone / one shared name-column width /
reset+forecast phrasing / history keys), 15 in-file tests.
Chunk 2 (`e34e0cee`): the `Alt-u` overlay rewritten as a projection of the
view — worst-first, one aligned line per limit, facts below the numbers,
forecast-gated sparkrows, spacers, a legend `Section`; `HeadingToned`
gains `label_tone` (bold); `caps::bar_track` degrades the gauges and both
shared draw sites route through it; configured thresholds threaded to all
five overlay call sites (§1.6).
Chunk 3 (`fb1e1ea3`): the panel Usage section projects the same view across
its three width tiers; help page updated; help ratchets green untouched.

## Design conformance (design.md, verified against the code)

- §1.1 density/separator — spacer between accounts only, tested at the edges.
- §1.2 dead rows — `Sparkrow` only where a forecast exists; density pinned by
  a section-count test.
- §1.3 alignment — one `name_w` measured in display cells across every
  selected window; overlay and panel both test bar-column equality across
  differently-named windows.
- §1.4 facts below numbers — both surfaces, tested.
- §1.5 plain language — minutes-first naming with model qualifiers;
  non-integral/unknown lengths pass the label through (no invented durations).
- §1.6 thresholds — `usage_view::build` tones against the caller's
  `warn_percent/crit_percent`; the old hard-wired `usage::tone` call is gone;
  badge/panel/overlay now read one source; regression-pinned in both
  renderers' tests.
- §1.7 degradable bars — `caps::bar_track` delegates verbatim on Full/Basic
  (byte-identity test-pinned), fills `bar_fill/bar_empty` on Ascii (no
  `U+2500..259F`, width invariant `bar+track==w` across levels/widths); both
  chokepoints (`draw_table`, `bar_segs`) routed; signature untouched.
- §1.8 legend is a trailing `Section::Heading`, never `ov.hint` — tested as
  the last section.
- §1.9 hierarchy — `HeadingToned.label_tone` drawn bold; existing
  constructions pass the dim slot (§3.3). Worst account leads in red, tested.
- §5 traps — each checked: no in-place sort of `model.usage` (`order` returns
  indices; statusbar badge and alert handler untouched on discovery order);
  `history_key` byte-identical to `detail::history_key` (and the host's
  sampler path in `actions.rs` still shares it); `Section::height` untouched
  (HeadingToned stays 1); `peak_window()` not `windows.first()`; unicode-width
  padding with explicit spaces; no `$HOME`/clock/I-O in core.

## Chunk "Unverified" items — resolved

- **Chunk 1, coverage:** measured. `just coverage` green (`core ≥95% lines`,
  3416 core tests); `usage_view.rs` specifically **428/428 lines (100%)**.
- **Chunk 1/2/3, clippy on test targets:** was in fact failing — 4×
  `cloned_ref_to_slice_refs` in `usage_view` tests, 2×
  `field_reassign_with_default` in the `usage_dash` / `panel usage` tests.
  **Fixed in `18b84e2c`** (mechanical; no behavior change); all-targets
  clippy now green workspace-wide.
- **Chunk 1, downstream compile / chunks 2-3 integration:** green — full
  `cargo nextest run` workspace post-merge: **6702 passed, 21 skipped**;
  `just smoke` green; `treefmt --ci` clean (the "1 changed" during one lint
  run was a concurrent `.thegn/pipeline` writer race, not source).
- **Chunk 2, e2e:** verified unnecessary — no usage frame in any baseline,
  and (beyond the design's check) the bold-dim `HeadingToned` change touches
  the status modal, which is also **not** in any snapshot (grepped
  `daemon`/`sessions` — empty). No re-record needed on either count.
- **Chunk 2, bold-dim side effect:** accepted as designed — §3.3 specifies
  the label draws bold; "unchanged on screen" in the chunk spec was wrong
  (dim+bold vs dim) but the design's intent rules, the readable-at-every-
  width test passes, and no snapshot covers it.
- **Chunk 3, `bar_segs` contract:** signature unchanged; chunk 2's degrade
  landed transparently (verified by reading `mod.rs` and the workspace run).

## Chunk 1's Cargo.toml deviation — accepted

`unicode-width` promoted from `[dev-dependencies]` to `[dependencies]`: the
chunk mandated `unicode_width::UnicodeWidthStr` and the design wrongly
claimed it was already a regular dep. Same crate/version, already in the
host's regular deps and the lock graph; no new external crate. There is no
compiling alternative that honours the "measure in display cells" invariant.

## Notes (non-blocking)

- **Design-side doc slip:** §3.5 says Full width shows "the absolute reset";
  that was inherited from the old module doc-comment, which never rendered an
  absolute reset (only a `/{len}` length fragment). The new code honestly
  drops the claim. No code action; the design text is wrong, not the branch.
- Nit: chunk 3 imports `Glyph` from `thegn_core::termcaps` rather than the
  new `crate::caps` re-export chunk 2 added for it. Same type, draw sites
  still resolve through `caps::glyph`; harmless either way.
- `run.rs` edits are parameter-only as designed (four `&model.usage_cfg`
  args, no logic).
- No `openspec/` file, no ratchet file (`test/*ratchet*`) changed;
  help-page frontmatter untouched; commit subjects match the chunk specs
  exactly; no conflict markers on the branch.

## Merge with main (lead addendum)

`git merge main` → `2cb6ea54`, auto-resolved (no source conflicts; pipeline
docs merged trivially). Full workspace gates re-run on the merged tree — all
green (numbers above). Note: local `main` moved again during review
(THE-73 fold, `6040fa1d`); its files (`session.rs`, `sidebar.rs`, `run.rs`)
do not overlap any THE-65 file, so a follow-up merge before landing is
trivial.

## Gates run by this review

`just quick` (lib/bin) · `cargo clippy --workspace --all-targets -D warnings`
· `cargo nextest run` workspace (6702) · `just smoke` · `just coverage`
(≥95% core; usage_view 100%) · `treefmt --ci` · scoped nextest for
`usage_view`/`usage`/`sections`/`status_modal`/`help`.
