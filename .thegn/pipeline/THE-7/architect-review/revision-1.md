# THE-7 architect revision 1

Verdict: REVISE. The implementation has the right decomposition and the
focused gates pass, but the following semantic gaps must be corrected before
this issue can land.

## 1. Make the user-theme resolver real at runtime

- `crates/thegn-core/src/config.rs:6282-6300` resolves every name through
  `theme_resolve::palette_with_config`, which calls only the built-in
  `theme::preset` path at `crates/thegn-core/src/theme_resolve.rs:10-21`.
  `Config::palette()` therefore ignores a valid local theme named by
  `[theme].preset`.
- `crates/thegn-host/src/run.rs:10491-10520` likewise applies
  `new_cfg.palette()` on reload and has no active-user-theme resolution path.
  The loaded `theme_users` vector is used only by the builder/cycle catalog.

Expected fix: keep filesystem access in the host worker, but add a pure
resolver API that accepts the loaded user-theme catalog and applies config
overrides after the selected user theme (with built-ins winning collisions).
Use it for initial catalog completion, normal runtime palette installation,
cycle, builder selection, and config reload. Add tests proving that a local
`[theme].preset` survives restart/reload and that overrides layer in the same
order for built-in and user themes.

## 2. Preserve config overrides while previewing and applying

- `crates/thegn-host/src/theme_builder.rs:120-124` initializes a selected user
  candidate with `theme.palette()` and `:375-382` repeats that behavior on
  selection; `cycle_catalog` at `:480-493` does the same. Existing
  `[theme.colors]`, `[theme.hues]`, `accent`, and `focus_border` values are
  silently bypassed in all three paths.
- `crates/thegn-host/src/theme_store.rs:247-301` and the duplicate CLI helper
  `crates/thegn-host/src/cmd/theme.rs:173-220` write every user-theme value
  into config on an apply. The builder passes `persist_theme = false` for a
  built-in, but `write_config` still materializes all colors/hues, freezing a
  built-in selection into a full override set. This is not the designed
  "select preset, preserve existing overrides" behavior.

Expected fix: resolve every candidate through the shared user/config resolver;
make persistence distinguish selection from explicitly edited overrides;
write only `[theme].preset` for a selection, and write the intentional edited
override set for an edited candidate. Remove the duplicated persistence
policy and add a comment-preservation test for built-in selection, user
selection, and token edit.

## 3. Fix the popup geometry and preview contract

- `crates/thegn-host/src/theme_builder.rs:579-589` requests a 26-row layer,
  but `layer::open_layer` clamps the interior to 21 rows on a normal 80x24
  terminal. `ACTION_ROW` is 22, so the Apply row is outside the rendered
  interior and cannot be mouse-clicked.
- At a terminal large enough to avoid that clamp, the preview begins at
  `:753-810` at `inner.y + TOKEN_ROWS + 2`, while the Apply row is at
  `:695-715` at `inner.y + 1 + ACTION_ROW`; the preview is drawn afterward
  and overwrites the Apply row on its second row. Most of the preview is
  also below the layer's interior.

Expected fix: choose a layout from the measured interior, with a visible
Apply row and a non-overlapping preview at 80x24 and at the normal large
layout. Add renderer/hit-test tests that assert the Apply row and at least
the representative preview rows are inside the same `box_rect` and do not
overlap. Keep all swatches on palette tokens and retain `open_layer` as the
placement/hit-test source.

## 4. Do not lose worker requests during watcher debounce

- `crates/thegn-host/src/theme_store.rs:129-134` drains the request channel
  for 200ms after `Work::Changed` and discards every received value. A queued
  `Request::Save`, `Import`, or `Apply` can therefore be swallowed whenever a
  watcher event is coalesced with it; the UI then waits forever for a result.

Expected fix: debounce only change notifications while preserving and later
processing all request messages. Add a worker test that queues Changed plus
Save/Import/Apply and asserts one result for each request.

## 5. Enforce hostile-name safety at the core boundary

- `crates/thegn-core/src/theme_import.rs:211-216` accepts any non-empty Gogh
  `name`, including control characters. `crates/thegn-host/src/cmd/theme.rs:97`
  prints it, and `crates/thegn-host/src/theme_builder.rs:654` renders it as a
  catalog label. A JSON/YAML name containing ESC can therefore reach a
  terminal output path, contrary to the import safety contract.

Expected fix: reject control/non-printing name data in the substrate-free
model/import validator (or apply one shared safe display policy before every
render/CLI emission), and add a hostile-name test. Do not rely on the hex
validator alone; the unsafe field is metadata.

## 6. Surface the required built-in-collision warning

- `crates/thegn-host/src/theme_store.rs:184-211` records malformed-file
  warnings but not a valid user theme whose `meta.name` collides with a
  built-in.
- `crates/thegn-host/src/theme_builder.rs:459-477` and
  `crates/thegn-host/src/cmd/theme.rs:47-55` silently omit the colliding user.

Expected fix: retain built-in precedence, but emit one deterministic warning
including the shadowed theme path/name through the existing store status path
and CLI warning path. Add a collision test.

## 7. Synchronize the OpenSpec change with the decided scope

`openspec/changes/add-theme-builder-overlay/specs/theming/spec.md` and its
proposal/tasks still claim base16 import and export, while the architect
design explicitly cuts both and the implementation does not provide them.
Update the change artifacts to the decided Gogh-only/no-export scope (or
implement the claimed surface), then run strict OpenSpec validation.
