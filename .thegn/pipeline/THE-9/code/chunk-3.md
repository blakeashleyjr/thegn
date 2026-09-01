# Chunk 3 — opt-in bottom widget, detail bridge, and help

## Files touched

- `crates/thegn-host/src/statusbar_badges.rs`
- `crates/thegn-host/src/chrome.rs`
- `crates/thegn-host/src/statusbar_fit.rs`
- `crates/thegn-host/src/detail.rs`
- `crates/thegn-core/src/config.rs`
- `config/config.toml.example`
- `docs/help/bars.md`
- `docs/help/sidebar.md`
- `docs/help/merge-queue.md`
- `CHANGELOG.md`

Do not touch sidebar data/render/input files; chunk 2 owns those.

## Approach

Run after chunk 1; it may run in parallel with chunk 2 because paths are
disjoint. Refactor the existing merge-queue badge renderer to produce the same
compact, core-policy-driven segments for `BarItemId::Widget("mq")`. Remove its
unconditional default insertion from `statusbar_items`; do not change the
default widget array. Preserve existing scoped behavior and use the existing
caps/palette seams. The widget's detail route must call the existing unified
detail surface, not duplicate queue rows. Keep legacy enum plumbing only when
needed by existing tests/compatibility; it must not be emitted by the default
bar.

Add `mq` to the documented built-in widget ids in the Rust config docs and
`config/config.toml.example`. This is an existing-array value, not a new config
key, so do not add an env-overlay entry. Add a priority for the ordinary widget
in the existing statusbar fit policy and test that it sheds correctly.

Update the bars/sidebar/merge-queue help prose to describe the workspace token,
the right-panel destination, and `[bars] bottom_right = ["mq"]` opt-in. Keep
the existing action/context frontmatter unless a ratchet proves a real new
action claim is required. Update the Unreleased changelog and explicitly state
that e2e baselines are not re-recorded.

Verify, without adding entries, the env-overlay, completion-slot,
control-schema, color/glyph, and help ratchets. There is no new capability or
bindable action in this chunk.

## Dependencies / overlap

Serial after chunk 1 because the renderer uses the core rollup policy. It is
file-disjoint from chunk 2 and may run in parallel with chunk 2 after chunk 1
lands. No shared file edits are permitted.

## Tests to run

- `just quick thegn-core`
- `cargo nextest run -p thegn-core bars_config_defaults`
- `just quick thegn-host`
- `cargo nextest run -p thegn-host statusbar_badges`
- `cargo nextest run -p thegn-host statusbar_fit`
- `cargo nextest run -p thegn-host detail`
- `cargo nextest run -p thegn-host help`

Also run the repository's focused ratchet tests for env overlay, completion
slots, control schema, color/glyph literals, and help claims if they are not
covered by the package filters. Do not run e2e, `just test`, `just ci`, or a
full-workspace compile.

## Done criteria

- Default bottom bar no longer emits merge queue; `[bars]` can opt into `mq`,
  and activating it opens the existing unified merge-queue detail.
- Config docs, help pages, and changelog describe the final behavior; no new
  undocumented config key/action/context exists.
- Existing fit priorities and all other badges remain unchanged.
- All relevant ratchets remain unchanged or shrink only; no new literal debt.
- The coder commits early and finishes with this exact commit subject:
  `feat(the-9): make merge queue bar widget opt-in`
