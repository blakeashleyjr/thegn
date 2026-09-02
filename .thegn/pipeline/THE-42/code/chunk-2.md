# THE-42 chunk 2 — canonical docked Media panel

## Ownership and sequencing

This chunk is serial after chunk 1 because it consumes the reserved-kind/config
and media-policy contracts. It touches no chunk-1 files. The only dependency
is the public media API; file ownership is disjoint.

## Files to touch (exact paths)

- `crates/thegn-host/src/main.rs`
- `crates/thegn-host/src/run.rs`
- `crates/thegn-host/src/panel/mod.rs`
- `crates/thegn-host/src/panel/frame.rs` (only if the new media hit variant
  needs frame plumbing; do not duplicate layout/hit calculations)
- `crates/thegn-host/src/panel/section_keys.rs`
- `crates/thegn-host/src/panel/sections/media.rs`
- `crates/thegn-host/src/panel/media.rs` (new pure-ish panel state/hit mapping
  sibling, if that is the smallest module boundary)
- `crates/thegn-host/src/handlers/media_panel.rs` (new shared keyboard/mouse
  action handler)
- `crates/thegn-host/src/media_ctl.rs`
- `crates/thegn-host/src/media_watch.rs`
- `crates/thegn-host/src/media_overlay.rs` (remove the modal integration/file;
  retain reusable art decoding in `media_art.rs`)
- `docs/help/media.md`
- `docs/help/panel.md`
- `test/help-ratchet.txt`
- `test/help-prose-ratchet.txt`
- `test/help-panel-prose-ratchet.txt`
- `test/help-context-ratchet.txt`
- `test/glyph-literal-ratchet.txt` (only if removing an existing media literal
  or pinning a justified edge; new draw-site literals are forbidden)

Verification-only files, expected byte-identical because there are no new
actions, completion slots, external doors, or e2e recordings:

- `test/completion-slot-ratchet.txt`
- `docs/api/control-v1.json`
- `test/env-overlay-ratchet.txt`
- `test/muse/snapshots/panel_system__system/xterm__100x30__linux.txt`
- `test/muse/snapshots/glitch_hunt_panel_accordion__after/xterm__100x30__linux.txt`
- `test/muse/snapshots/glitch_hunt_panel_accordion__after/xterm__160x40__linux.txt`
- `test/muse/snapshots/panel_work__work/xterm__100x30__linux.txt`

## Approach

1. Make `Section::Media` the sole full media surface. Replace the current
   `MediaOverlay` state/render/key ownership (`run.rs` currently opens it at
   `:6827-6840`, renders it at `:12296-12299`, and owns its keys at
   `:14250-14265`) with panel state and a handler. Remove the centered modal;
   do not add `Anchor::At` or a corner fallback.
2. Expand `panel/sections/media.rs` into Normal/Half/Full list/detail rows.
   Use `PanelFrame` rows as the one layout/hit source. Keep data normalized:
   player/source rows, current now-playing detail, queue rows, and
   capability-gated controls. Queue and art deliveries must be identity-checked
   and must degrade silently when unavailable.
3. Add a compact media-specific hit target only where a painted transport
   control needs a direct mouse action. Keep source and queue rows as
   `PanelHit::Row(Section::Media, index)`. Mouse selection and keyboard
   selection must converge on one handler; the handler maps to existing
   `MediaOp`/`Action` IDs and sends work to the established off-loop channel.
4. Route `Alt-m`/`Action::MediaOpenPanel`, badge click, Media Enter, and the
   existing section transport keys to focus/use the docked section. Preserve
   disabled-media status behavior. Do not add action IDs; do not overload the
   sandbox `s/r/l` stop/restart/log meanings. Media retains `s` shuffle and `L`
   loop plus the existing `media-*` action registry.
5. Simplify `media_watch::drain_snapshots` so it updates the model/panel state,
   not an overlay. Preserve the existing repaint coalescing and off-loop
   channel/waker behavior. The panel’s open/visible state may enable position
   repaint exactly as today; closed media must not create a new periodic wake.
6. Update help prose/frontmatter only for the existing Media actions and
   `panel:media`. Explain list/detail keyboard and mouse behavior, Spotify’s
   current MPRIS route and reserved kind, and why visualization is intentionally
   absent. Keep all help ratchets clean in this commit.

Use theme slots and `caps::active_glyphs()` for all transport/status symbols.
The panel must render correctly at narrow widths with art omitted first.

## Tests to run

Run only scoped checks; do not run `just test`, `just ci`, a workspace build,
e2e, or the built binary.

- `just quick thegn-host`
- `cargo nextest run -p thegn-host media`
- `cargo nextest run -p thegn-host panel`
- `cargo nextest run -p thegn-host help`

Add focused unit tests for:

- Normal/Half/Full media row projection and capability-hidden controls;
- keyboard and mouse hit-table equivalence for source, queue, and transport
  targets;
- `Alt-m`, badge click, and Media Enter all focusing `Section::Media`;
- disabled media and missing-provider degradation;
- no modal/overlay ownership remaining and no new wake source at idle.

If a `thegn` command is needed, set `XDG_STATE_HOME` to a fresh temporary
directory. Do not invoke a migration or touch the live state DB.

## Done criteria

- The System→Media panel is the canonical now-playing surface, with list/detail
  behavior across panel widths and a frame-derived mouse hit table.
- `Alt-m`, `media-open-panel`, Media Enter, badge click, keyboard transport,
  and mouse transport all reach the same docked panel/handler path.
- No centered media modal, duplicate overlay state, or `Anchor::At` popup is
  left behind. All provider work remains off-loop and idle remains blocking.
- Existing media action IDs, one capability catalog, completion slots, control
  schema, env-overlay ratchet, and help ratchets remain clean; any necessary
  ratchet edit is made in this commit with its reason.
- `docs/help/media.md` and `docs/help/panel.md` document the actual panel,
  controls, Spotify reserved posture, and visualization rejection.
- The listed e2e snapshots are not re-recorded or changed.
- Commit exactly with subject: `feat(the-42): dock media in the panel`
