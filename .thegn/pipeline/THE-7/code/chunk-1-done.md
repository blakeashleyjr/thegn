# THE-7 chunk 1 — core theme and Gogh contracts done

Implemented the chunk-1 core-only scope on `tg/the-7-theme-builder-popup`.

## Changes

- Added the closed, versioned `UserTheme` TOML model with metadata, editable
  surface/text/accent/focus roles, and all eight semantic hues.
- Added validated TOML round-tripping and palette serialization helpers.
- Added the bounded, substrate-free Gogh YAML/JSON parser with strict fields,
  scalar-only YAML handling, size limits, typed errors, and hex validation.
- Added the deterministic `Ansi16` Gogh mapper: background/foreground/cursor
  map to `bg0`/`text`/`focus`, ANSI pairs select by contrast, all sixteen ANSI
  values participate, and orange/magenta are derived blends.
- Added the shared palette resolver and delegated `Config::palette_with_preset`
  and accent parsing to it without changing the config schema or existing
  defaultish accent/focus behavior.
- Every user/imported palette runs through `extend_palette`; contrast audit
  compatibility is covered by core tests.

## Verification

- `XDG_RUNTIME_DIR=/tmp TMPDIR=/tmp just quick thegn-core` — passed.
- `cargo nextest run -p thegn-core theme_import` — 3 passed.
- `cargo nextest run -p thegn-core theme_user` — 2 passed.
- `cargo nextest run -p thegn-core palette` — 5 passed.

## Unverified

- Full workspace gates (`just test`, `just ci`, coverage, and full builds) were
  intentionally not run per the chunk dev-loop policy.
- Host overlay, CLI, filesystem worker, and e2e snapshot cases remain for
  chunks 2–3 and the later test pass. The deferred cases are:
  `theme-builder-open`, `theme-builder-edit-preview`,
  `theme-builder-cancel-reverts`, `theme-builder-gogh-import`,
  `theme-builder-save-reload`, and `theme-builder-small-terminal`.

## Commits

- Early checkpoint: `996a0ce0` (`wip(the-7): add core theme contracts scaffold`)
- Final code commit subject: `feat(the-7): add core theme and Gogh import contracts`
