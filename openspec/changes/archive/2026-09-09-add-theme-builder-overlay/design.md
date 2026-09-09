# Design — theme builder overlay (accepted v1)

## Decisions

1. The builder is an ordinary boxed `Layer`; it does not introduce a widget
   framework or a second render path.
2. Preview mutates only runtime palette state. Cancel restores the saved
   selection; confirm persists through the comment-preserving config editor.
3. Every swatch and sample uses palette roles/tokens and the existing contrast
   audit. Imported text is never emitted to the terminal as styling bytes.
4. Theme discovery, parsing, import, and all filesystem writes execute in the
   background `ThemeStore`. Results return through a channel and terminal
   waker; the UI loop performs no filesystem I/O.
5. User-theme names are confined/slugged, inputs are size-capped, corrupt files
   warn and skip, and built-in names win collisions.
6. Gogh local-file import is the only v1 import format. There is no network
   catalog or export surface.

## Reload behavior

While preview is active, a config reload does not overwrite the candidate
palette. Confirm writes the key the runtime reads (`[theme] preset`) and applies
token overrides consistently; cancel restores the prior resolved palette.

## Plugin boundary

The declared `Theme` plugin enum value is not a wired runtime extension point.
THE-107 owns whether and how it should become one.
