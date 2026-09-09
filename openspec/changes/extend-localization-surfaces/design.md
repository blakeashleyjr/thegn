# Design — complete localization surfaces

## Current architecture

`thegn-core` owns locale selection, Fluent lookup/fallback, parity analysis,
pseudolocale generation, and pure formatting. Host startup initializes the
locale once, and host surfaces use a small adapter. The currently migrated
statusbar and palette are a proof of the seam, not a declaration that chrome is
localized.

## Milestone decisions

### Native chrome

en-US is the schema and every shipped locale has exact key parity. Each native
surface moves literals to stable semantic keys, with the shrink-only debt list
preventing regressions. Layout tests use cell width and the pseudolocale. Doctor
reports resolved locale and parity/coverage from the same core fold.

### Time and date

One pure formatter family owns relative age, duration, month/weekday names, and
the name-producing portions of configured date formats. Fluent selects plural
forms. CLI callers may use the same algorithms pinned to en-US.

### Bidi

Chrome remains logical-order LTR until a terminal-bidi design exists. User-
controlled branch, path, host, and similar strings have bidi control characters
neutralized before becoming thegn-drawn chrome; pane bytes remain untouched.

### Help

English pages are canonical. A localized page tree may override individual
pages; missing pages fall back to English. Action, prose, context, registration,
and orphan gates inspect only canonical content. Generated config reference
stays English.

### CLI

JSON schemas/values owned by thegn, exit codes, doctor result vocabulary, and
`keys list` remain byte-stable across locales. Human CLI prose stays English in
this phase.

### Plugin strings

Plugins register stable namespaced keys and their fallback text through a
versioned host contract. The host chooses locale/fallback and sanitizes output;
plugins do not mutate the host's global Fluent loader or supply terminal escape
sequences. THE-106 must document the exact version/scope support matrix.

## Runtime invariants

Locale resolution happens once at startup. Translation and formatting are pure
render inputs, so no new timer, wake source, filesystem watch, or render-plan
decision is introduced.
