# Theme builder overlay (accepted v1)

Linear: THE-7 (Done)

## Why

Users needed an in-product way to preview, select, edit, import, and save token
themes without bypassing the existing palette and configuration contracts.

## What Changed

- Added a bindable theme-builder overlay with live preview, confirm, and cancel.
- Added editing for the existing color/hue token vocabulary with inline contrast
  findings.
- Added safe user-theme loading/saving under the configured themes directory.
- Added local-file Gogh import and non-interactive `thegn theme list|set|import`.
- Routed directory scans, imports, and writes through background `ThemeStore`
  work and the normal result/waker path.

## Accepted v1 boundary

This version does not export themes, fetch remote catalogs, import Base16, add a
retained-mode GUI, or make `ExtensionPoint::Theme` a runtime plugin surface.
The bounded decision about plugin theme/sidebar/key-zone extension points is
THE-107 (`decide-plugin-ui-extension-surfaces`).

## Impact

- Specs: `theming`.
- Config: comment-preserving writes to `[theme] preset`, `[theme.colors]`, and
  `[theme.hues]`; user theme files use the same token vocabulary.
- Runtime: palette changes remain a full chrome redraw; no idle polling.

## Archive status

The accepted THE-7 v1 is delivered and archive-ready. Later import/export or
plugin-theme work requires a separate change.
