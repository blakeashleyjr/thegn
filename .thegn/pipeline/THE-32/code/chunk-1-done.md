# THE-32 chunk 1 completion

Implemented the core submodule models and configuration from the chunk spec.

## Delivered

- Added strict `.gitmodules` and `git submodule status` parsing, gitlink diff parsing, bounded summaries, pointer classification, component-aware boundaries, row formatting, and typed conflict formatting in `thegn-core::submodule`.
- Added `[git].submodules` with `auto`/`off` parsing and `THEGN_GIT_SUBMODULES`, plus `ui.sidebar_show_submodules = true` and the config example/ratchets.
- Added the submodule glyph with Unicode/ASCII fallbacks.
- Classified mode `160000` patch entries and forge diff entries as atomic submodules; selection and transformation reject partial submodule hunks.
- Carried typed submodule conflicts through fold plans and agent-task prompt variables while preserving the existing default prompt text.
- Updated the required config coverage and enum-count ratchets.

## Verification

- `XDG_RUNTIME_DIR=/tmp TMPDIR=/tmp RUSTC_WRAPPER= just quick thegn-core` — passed.
- `RUSTC_WRAPPER= cargo nextest run -p thegn-core submodule` — 12 passed.
- `RUSTC_WRAPPER= cargo nextest run -p thegn-core patch` — 88 passed.
- `RUSTC_WRAPPER= cargo nextest run -p thegn-core fold` — 42 passed.
- `RUSTC_WRAPPER= cargo nextest run -p thegn-core --test env_overlay_coverage` — 2 passed.
- Focused config enum-ratchet and termcaps tests passed.
- `git diff --check` and repository pre-commit checks passed.

## Unverified

- Full-workspace compilation/tests and e2e were not run, per the chunk dev-loop policy.
- The broad `config` filter had one unrelated DNS forwarding test fail because the sandbox denied socket access; the targeted config ratchets passed.
- Downstream host consumers of the new forge `DiffFile::is_submodule` field were not compiled in this core-only chunk; their integration remains for the subsequent view work.

Commit: `4a7e5db0` (`feat(the-32): add core submodule models and config`)
