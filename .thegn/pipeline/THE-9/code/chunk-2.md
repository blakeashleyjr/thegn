# Chunk 2 — workspace token rendering, hit testing, and activation

## Files touched

- `crates/thegn-host/src/main.rs`
- `crates/thegn-host/src/sidebar_mq.rs` (new)
- `crates/thegn-host/src/sidebar_view.rs`
- `crates/thegn-host/src/handlers/sidebar_keys.rs`
- `crates/thegn-host/src/handlers/sidebar_mouse.rs`
- `crates/thegn-host/src/run.rs`

Do not touch statusbar/config/help files; chunk 3 owns those.

## Approach

Run after chunk 1. Register a small `sidebar_mq` host adapter. It converts the
core rollup into palette/capability-resolved segments and reports the measured
token span. Blocked is red, working amber, populated dim; count is dropped
before the marker, then the whole token is hidden. Use `caps::active_glyphs()`
and `Tok`; add no glyph/color literals.

Extend the existing `SidebarPlacement`/`RowHit` shared geometry with an
optional token x-range. In full mode, compose the workspace header right side
as `merge-queue token` followed by the existing warm token. Keep the header one
row and preserve the current left label/caret floor. In rail mode, do not paint
the token; tint only the existing workspace initial for blocked/working using
semantic palette tokens. Paint and hit-test must consume the same placement
geometry, including separator gaps and narrow widths.

Add `SidebarOutcome::OpenMergeQueue { repo_path }`. A token click must be
checked before ordinary workspace activation but after caret/Ctrl-click guards.
The workspace context menu gets an `Open merge queue` item; its existing `m`
menu and Enter path must return the same outcome. The run dispatcher activates
the workspace, selects the existing Work → Merge queue section, and uses the
existing dirty/relayout/hydration seams so queue changes remain chrome/model
damage and the render plan still reaches `Full`.

Do not create a new keymap action, capability, panel section, queue query, or
wake source. Keep all mutation actions and existing row-menu entries intact.

## Dependencies / overlap

Serial after chunk 1 because this consumes `MqRollup` and `SidebarRow` fields.
File-disjoint from chunk 3; chunk 2 owns all sidebar/input/event-loop files,
while chunk 3 owns bars/detail/docs/config. No other chunk may edit these
paths while this chunk is in flight.

## Tests to run

- `just quick thegn-host`
- `cargo nextest run -p thegn-host sidebar_view`
- `cargo nextest run -p thegn-host sidebar_mouse`
- `cargo nextest run -p thegn-host sidebar_keys`
- `cargo nextest run -p thegn-host render_plan`

Tests must cover token fit and marker fallback, rail/full behavior, shared
paint/hit x-ranges, separator-gap safety, token click routing, keyboard menu
routing, workspace switching, and chrome/full invalidation. Do not run e2e or
a full-workspace build.

## Done criteria

- Full workspace headers show the scoped token before warm; narrow full sidebars
  degrade count→marker→hidden; rail remains legible and ASCII-capable.
- A token click and the context-menu item both open the existing right-panel
  Merge queue section after selecting the correct workspace.
- Existing Enter/caret/drag/Ctrl-click behavior is unchanged.
- No new glyph/color literal, keymap action, capability, provider, or ratchet
  debt is introduced.
- The coder commits early and finishes with this exact commit subject:
  `feat(the-9): add workspace merge queue token`
