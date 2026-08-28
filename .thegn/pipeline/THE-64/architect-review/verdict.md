# THE-64 — architect review verdict

**Verdict: APPROVED** — no revision chunk. One merge commit applied by the
reviewer (`7ce2b380`), zero code corrections needed.

Reviewed: `git diff main...HEAD` after merging `main` into
`tg/the-64-sidebar-distinction` (lead addendum), against
`architect/design.md`, both chunk specs, and repo standards
(ARCHITECTURE invariants, ratchets, dev-loop policy, help/coverage gates).

---

## 0. Merge of main (lead addendum) — done by reviewer

`main` had moved (THE-70 sidebar/doctor, THE-74 pipeline board + derived
sidebar rows, THE-83 agents/model/env, bundled skills). Merged with two
conflicts, both additive, both resolved by keeping both sides:

- `CHANGELOG.md` — THE-64's `### Changed` section and main's two `### Added`
  sections now coexist under `[Unreleased]`.
- `crates/thegn-host/src/sidebar_view.rs` — THE-64's test block and THE-74's
  test block both appended to the same module; markers removed, treefmt applied.

**Merge interactions checked:** THE-74's `PipelineGroup` / `PipelineLane` /
`PipelineWorktree` compose arms are byte-identical to main's (already toned
plain + faint caret = tier 2, so the retiered `Folder` arm does not drag them);
`PipelineGroup` is not in `row_bg`'s `header` predicate, so it is correctly
unbanded. `RowKind::PipelineGroup`'s caret sits at `rect.x + 3` inside the
`Folder` arm — unchanged, so THE-64's folder caret contract still holds.

## 1. Design conformance — full

Chunk 1 (`7eaec5c8`): exactly the two specified files; `sidebar_dividers`
field/default/test/example-doc all per spec; `config_validate.rs`'s `90`
`config_enum` pin untouched; no ratchet entries.

Chunk 2 (`149bb700`): Parts A–D all per spec —

- **Tiers**: workspace/host label `S::Accent`+bold; plain-git workspace gets
  `◆` in accent; `dir` arm keeps `gl.dir` moved to accent; host glyphs stay
  `S::Dim` (local-vs-remote meaning); folder drops bold, glyph to `S::Faint`,
  count split out in `S::Faint`; `row_bg` header predicate is now
  `Workspace | TerminalHost` only; `SectionHeading` untouched.
- **Gap**: `lead_gap_rows` / `dividers_on` verbatim; height pass, compose
  pass, scroll geometry and hit-testing all read the one helper; lockstep
  `debug_assert_eq!` sees the untrimmed vector; clipped-tail trim generalized
  to `lead_gap > 0` **plus** a post-trim recompute of `placement.lead_gap`
  from the surviving leading blanks (a correct refinement beyond the spec —
  paint and the caret cell then agree with what is actually on screen).
- **Paint split** (design §3.3's required split): gap renders on
  `Tok::Slot(S::Panel)`, body on `p.bg`; cursor bar starts at `p.lead_gap`.
- **Caret guard** (Part C): `my >= hit.y + hit.lead_gap` on the press path,
  with a real two-sided test. Workspace caret still `rect.x + 4`, folder
  `rect.x + 3`.
- **Docs** (Part D): help page "Reading the tree" section (frontmatter
  untouched → help ratchets unaffected); CHANGELOG entry states plainly that
  e2e baselines were NOT re-recorded; commit body carries the FRAME CHANGE
  note; the openspec delta was amended exactly per design §5 deviation 1
  (gap click resolves to the owning header; caret cell inert on the gap
  line; drag clause unchanged).

No color or glyph literal at any draw site; no `GlyphSet` field; file sets
match the chunk specs exactly; both commit subjects exact.

## 2. Unverified sections — disposition (all were mine to verify)

| done-file claim                            | disposition                                                                                                                                                                              |
| ------------------------------------------ | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `just quick thegn-core` / `thegn-host`     | **Re-run post-merge: clean.** Covers every workspace crate's lib/bin transitively (svc, gtui-embed, …).                                                                                  |
| scoped nextest suites                      | **Re-run post-merge: config_ui 9/9, config_example 2/2 (drift gate green after THE-83's keys), sidebar 250/250, chrome 87/87, sidebar_mouse 20/20.**                                     |
| clippy on new test code not run            | **Verified: `cargo clippy -p thegn-host --tests` — clean.**                                                                                                                              |
| `just term-check` not run                  | **Verified: all green** — kitty/truecolor, 16-color+ascii, mono, 256, ascii-glyph, 16-color+full-glyph. The design's "ladder survives quantization" claim holds in all six environments. |
| `openspec validate --all --strict` not run | **Verified: 169 passed, 0 failed** — the amended delta is shape-valid.                                                                                                                   |
| full suite / coverage / e2e                | Owned by the pre-push hook / CI per dev-loop policy; see §3.                                                                                                                             |

## 3. Flagged follow-ups (deliberate, not revision blockers)

1. **e2e baselines are stale on this branch.** Deferred by design §8 and
   documented in the CHANGELOG and commit body — but it must actually happen:
   `just e2e-update` in a deliberate pass **after landing, on main, once**,
   so it also absorbs the frame movement main's THE-74 board introduced
   (main's snapshots were already un-re-recorded there). Until then
   `just ci-local` / the e2e gate cannot be green; that is known and paid
   for knowingly.
2. `just test` / coverage / clippy all-targets run at pre-push, as designed;
   scoped evidence above gives high confidence they pass.

## 4. Verdict

**APPROVED.** The implementation is the design, line for line, with two
justified refinements (post-trim `lead_gap` recompute; digit assertions
strengthened to include the diamond). Merge to the queue may proceed; the
only scheduled debt is the single e2e re-record pass on main.
