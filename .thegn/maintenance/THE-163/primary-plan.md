# Primary review + greenlight — THE-163

Reviewing row 583's investigation. **APPROVED to implement as proposed.**

The three-level classification is the right shape and the placement discipline
is exactly what the brief asked for:

- `None` — no visible change, all damage channels clear. This is what makes
  "repeated identical updates generate no frame" structural rather than a
  conditional someone can later break.
- `StatusbarContent` — changed `View`, stable rendered width and placement, sets
  only the bounded statusbar region.
- `Structural` — add/remove, load/respawn/disable/exit, or changed width or
  placement, retains the broad chrome path so `draw_statusbar` recomputes
  fitting and cluster placement.

Endorsed specifically: classification lives in a sibling module
(`plugin_damage.rs`), `run.rs` gets only the mapping plumbing, and
`render_plan::Damage` is extended just enough to separate a statusbar-only
region from the existing `bars` channel that also repaints the masthead.

## The width comparison is the hinge — get it exactly right

`StatusbarContent` is only sound while rendered width **and** placement are
unchanged. If a content-only classification ever fires on a width change, the
statusbar shows stale neighbouring segments — visible corruption, not a missed
repaint. So:

- Compare the **rendered** width (post-degradation, post-fit), not the
  pre-render string length. A glyph or color fallback can change width.
- When in any doubt, classify `Structural`. An unnecessary full repaint is a
  performance cost; a wrong content-only classification is a correctness bug.
- Test both directions explicitly: a same-width content change stays bounded,
  and a width-changing content change escalates to structural.

## Invariants (these have tests in `just test`)

- `render_plan::plan` stays **pure** — give it damage as input, never let it
  consult plugin state.
- 0% idle: an unchanged status must reach `Skip`, not a cheap frame.
- Extend the exhaustive `render_plan` unit tests; do not weaken them.
- No color or glyph literal at a draw site — `caps::active_glyphs()` and the
  `wire.rs` chokepoint only.

## Unchanged

THE-165 and THE-161 are unlanded and are not prerequisites — do not implement
generation-tagging or output bounding. Do not grow `run.rs`.

## Validation

`cargo check -p thegn-host --all-targets` plus focused `render_plan` and plugin
tests. **If `nix develop` fails inside your sandbox** — the pipeline env mounts
`/nix/store` read-only, which breaks it — report that and stop rather than
claiming ready; the primary runs them.
