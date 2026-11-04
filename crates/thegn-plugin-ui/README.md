# Shared plugin UI engine

`UiEngine` is the host-side controller/renderer for Plugin API 0.4.
It depends only on the pure wire model, `tg-kit`, and text/date helpers. It owns
no processes, filesystem, network clients, timers, terminal, or RPC IDs.

Apply a validated `AppView` with `replace_view`, then call `prepare(area)` outside
painting. Dispatch returned typed `AppEvent` viewport intents through the host
transport. Input returns a result and typed intents; painting only reads prepared
state. `cursor()` uses the prepared rectangle's coordinates; an `AppTile` adapter
converts these to tile-relative coordinates exactly once. Reset viewports on a
new process generation even when the widget identities and sizes are unchanged.

Editing supports grapheme-safe movement, selection, word/line navigation,
replacement paste, undo/redo, multiline text, searchable choices, toggles, and
raw integer/date/RFC 3339 datetime values. Tab traverses controls, Enter advances
single-line fields or inserts a multiline newline, Ctrl+Enter validates/submits,
and Escape closes a choice popup before cancelling a form. Shift+F10 opens
contextual actions; Space toggles table/tree selection and Ctrl+A selects all.

Forms retain drafts by state scope, widget ID, and form instance. Refresh updates
untouched fields while preserving dirty values. Acknowledgments carry the
original scope, instance, version, and submitted values; later typing survives.
Integer canonicalization only updates the editor when the accepted version is
still current. Removed dirty forms and fields are inspectable through the plugin-app-drafts action (F2 by default);
Ctrl+Delete explicitly discards the selected recovery. Unknown request outcomes
are separately owned by the host transport and exposed through F3.

Fields are limited to 32 KiB, undo to 64 transactions per editor, and all editor
histories—including choice-search text—share a one MiB budget. Complete encoded
events reserve four KiB for the bounded RPC envelope. Retained drafts plus
transport-owned submitted snapshots share an eight-record/eight-MiB limit.
Rejected edits preserve the original text and display a diagnostic. Diagnostic
messages strip terminal controls and are limited to eight KiB.

Weighted splits collapse to the focused branch when minimum sizes do not fit.
Grid viewports report their actual visible region, including zero size when
hidden. Tables honor column widths and clip without allocating huge padding;
trees preserve stable selections and expansion. Theme updates use `set_theme`.

Run `cargo test -p thegn-plugin-ui --offline` for deterministic editor, controller,
layout, malicious-input, and buffer checks.
