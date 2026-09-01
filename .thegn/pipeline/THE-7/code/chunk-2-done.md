# THE-7 chunk 2 completion

Implemented the host-side theme builder popup and live preview.

- Added the off-loop `ThemeStore` with bounded theme scanning/imports, a
  non-recursive debounced watcher, validated atomic saves, and asynchronous
  config/theme apply writes.
- Added the pure `ThemeBuilder` reducer and boxed modal renderer with built-in
  plus user catalog handling, token editing, save/import fields, paste
  sanitization, cancel reversion, capability glyphs, preview palette roles,
  and the warning-only core contrast badge.
- Wired the handler and event loop for live palette updates, modal keyboard/
  mouse/paste capture, worker result draining, config-reload candidate
  reapplication, and full-damage modal guards.
- Added `ThemeBuilderOpen` with `Ctrl+Alt+Shift+t`; `CycleTheme` now uses the
  merged built-in/user catalog.

## Verification

All targeted checks passed with `RUSTC_WRAPPER=` and temporary runtime paths to
avoid the sandbox's sccache/runtime-directory restrictions:

- `just quick thegn-host`
- `cargo nextest run -p thegn-host theme_builder` — 4 passed
- `cargo nextest run -p thegn-host render_plan` — 20 passed
- `cargo nextest run -p thegn-host keymap` — 61 passed
- `cargo nextest run -p thegn-host 'platform_ratchet_tests::'` — 5 passed
- `cargo nextest run -p thegn-host 'caret_ratchet_tests::'` — 2 passed

## Unverified

- Help ratchets/help documentation are deferred to chunk 3, which owns those
  files and the new action's help claim.
- E2E snapshots and full-workspace gates were not run, per the chunk policy.
- No live `thegn` process or state database was exercised.

## Commits

- Incremental checkpoint: `18f1a955 wip(the-7): wire theme builder host scaffold`
- Final code commit subject: `feat(the-7): add live theme-builder popup`
