# Chunk 3 — chrome indicator, loop wiring, and help

Commit subject (exact): `feat(drawer): add statusbar presence indicator`

## Scope

Wire chunk 2 into the event loop and render a removable, capability-degrading
statusbar indicator. Keep paint and hit testing on one layout path, preserve
the existing drawer height/geometry and pane damage rules, and document every
new action/config key. Do not re-record e2e snapshots.

## Files touched

- `crates/thegn-host/src/run.rs` — register/drain the new drawer handler,
  dispatch cycle/pick/selection actions, populate the frame's drawer-bar
  snapshot, keep the drawer's existing geometry invalidation, and route the
  indicator's mouse hit to the same toggle transition.
- `crates/thegn-host/src/chrome.rs` — add the small pure drawer-bar state to
  `FrameModel`, resolve the `drawer` widget through existing caps/theme
  chokepoints, and integrate it with the shared left statusbar layout.
- `crates/thegn-host/src/statusbar_left.rs` — render and hit-test `drawer` as
  one atomic left-bar item, before keyhints, using the same span computation.
- `docs/help/drawer-and-corner.md` — claim and explain `drawer-cycle` and
  `drawer-pick`, picker-by-name behavior, worktree/global semantics, pooling,
  persistence, and the new `[[tools]]` drawer metadata.
- `docs/help/bars.md` — document the removable `drawer` widget, closed/open
  visual states, click behavior, and narrow-terminal priority.

## Approach and invariants

The default `[bars] bottom_left` order is `help`, `drawer`, `keyhints` so the
presence hint is retained when contextual keyhints shed at narrow widths. The
widget always has the built-in files occupant to report, even if no configured
tool is valid. Closed is Dim; open is Accent and includes the active stable
label; show a count only when more than one valid occupant exists. Resolve the
glyph through `crate::caps::glyph(Glyph::Folder)` and colors through existing
`col(S::...)` slots. Do not add raw Unicode/ANSI literals or vendor labels at a
draw site.

Use one pure widget builder for `left_layout` and `left_item_spans`, with an
atomic span. Clicking it invokes the existing files-drawer toggle for the
current scope. It is not a new focus zone; keyboard access is via the existing
files action and the new palette-visible cycle/pick actions. Keep drawer output
damage as pane damage and drawer open/close/occupant changes as the existing
chrome/full invalidation.

When the drawer owns focus, global actions still dispatch before pane bytes.
The pending drawer picker must be handled before generic palette action lookup,
and Escape must cancel without changing the persisted state. The loop drain
must discard stale `(scope-key, occupant-id)` results.

## Overlap/dependency

Depends serially on chunks 1 and 2. It owns `run.rs` and chrome files, so no
other chunk may edit those files concurrently. There is no file overlap with
chunks 1 or 2. The handler/palette APIs from chunk 2 are the only integration
boundary; do not duplicate their state transitions in `run.rs`.

## Tests to run

- `just quick thegn-host`
- `cargo nextest run -p thegn-host chrome`
- `cargo nextest run -p thegn-host statusbar`
- `cargo nextest run -p thegn-host drawer`
- `cargo nextest run -p thegn-host help`

Run `git diff --check` and the applicable help/env/completion/control ratchets.
Do not run e2e or re-record snapshots. Do not invoke the built binary or touch
the live state DB; any invocation must set `XDG_STATE_HOME` to a fresh temp
directory.

## Ratchet and snapshot criteria

- `test/help-ratchet.txt`, `test/help-prose-ratchet.txt`, and
  `test/help-panel-prose-ratchet.txt` remain empty/valid after the two action
  docs are added; update only shrink-only entries if the ratchet requires it.
- `test/env-overlay-ratchet.txt` contains the structured tool metadata pins
  from chunk 1; no env knob is added.
- Completion-slot and control-schema snapshots remain unchanged because this
  adds no CLI slot or control route. If a scoped ratchet exposes an unrelated
  pre-existing drift, report it rather than weakening the pin.
- List the eight affected chrome/bar snapshot paths in the handoff, but do not
  modify or re-record them in this issue.

## Done criteria

- The indicator is visible by default, removable via `[bars].bottom_left`,
  honest on ASCII/non-color terminals, and mouse-clickable with no hit-test
  drift.
- Action dispatch, picker selection, worktree switching, global reuse,
  process exit, and existing drawer height/containment behavior all use the
  shared chunk-2 lifecycle.
- Help pages mention every new action and every new config key; help ratchets
  pass without adding undocumented debt.
- No e2e or snapshot re-recording was performed; the affected snapshot list is
  recorded in `architect/design.md`.
- `git diff --check` passes.
- Commit exactly as: `feat(drawer): add statusbar presence indicator`.
