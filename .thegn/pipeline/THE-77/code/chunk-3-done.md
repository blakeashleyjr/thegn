# THE-77 chunk 3 — completion summary

**Commit:** `db4e9dbd` — `docs(the-77): document the reserved db/debug panel sections, emptying the help-context ratchet`
**Branch:** `tg/the-77-arch-audit`

## What was done

Exactly the chunk spec; no other files touched.

### `docs/help/panel.md`

- Added `panel:db` and `panel:debug` to the `contexts:` frontmatter array
  (same multi-line bracket style; the frontmatter parser explicitly supports
  it — `frontmatter.rs` "prettier's multi-line bracket style" branch).
- The **system** tab bullet in the tab list now ends "— plus two **reserved**
  stubs, `debug` and `db` (see below)" — visible reservation, not silent
  padding.
- New subsection at the end of "The system tab, section by section":
  **"### db and debug — reserved placeholders"**. It states, matching what
  the code actually renders (`panel/sections/misc.rs` `db()` / `debug()`):
  db shows "no database detected" over "db introspection not wired yet";
  debug shows "no session", an empty `BREAKPOINTS` list ("none set"), and
  "debugger integration not wired yet". It says plainly they have no keys
  or behaviour behind them and do not appear in the built-in accordion.

### `crates/thegn-host/src/help/pages.rs`

- `context_pages_resolve` updated: `panel:debug` and `panel:db` now assert
  `Some("panel")` (panel.md claims them), with a comment pointing at the new
  "Reserved placeholders" section instead of the ratchet file.
- The "unclaimed context lands on index, never nowhere" property is kept,
  now probed with `panel:not-a-section` — a key _outside_ the vocabulary.
  Rationale in the comment: after this chunk every vocabulary key is claimed
  (the emptied ratchet enforces that), and frontmatter validation
  (`UnknownContext`) makes non-vocabulary keys permanently unclaimable, so
  the probe can never rot as pages grow.

### `test/help-context-ratchet.txt`

- Deleted the `panel:db` and `panel:debug` entries; the header comment block
  is intact and the file is now comments-only (the terminal empty-allowlist
  state the header describes).

## Verification (scoped, per dev-loop policy)

- `cargo nextest run -p thegn-host help` — **71/71 passed**, including
  `help::ratchet_tests::every_panel_context_has_a_documentation_page` (the
  two-directional context ratchet), `every_zone_has_a_documentation_page`,
  both action/prose ratchets, and `help::pages::context_pages_resolve` +
  `every_help_page_is_registered`.
- `cargo nextest run -p thegn-host help::pages` — 5/5 passed.
- `just quick thegn-host` — clean (clippy, lib/bin only).
- pre-commit ran `treefmt` on the changed files — Passed.
- `git status` before commit showed exactly the three spec'd files; commit
  contains exactly those three.

## Unverified

- **e2e not run** (forbidden by the lead addenda). Static analysis of
  `test/muse/specs/*.yaml`: no spec renders the panel help page's body (no
  F1-overlay or docked panel.md frame is snapshotted), and db/debug are not
  in `SECTION_ORDER`, so no accordion snapshot can show them — no re-record
  should be needed. Not confirmed by an actual `just e2e` run.
- The claim that db/debug are not openable in the UI today is verified by
  reading code (`SECTION_ORDER` excludes them; `Section::from_key` searches
  only `SECTION_ORDER`, so `[panel] sections` can't summon them; grep found
  no other assignment to `PanelUi::open`), not by driving the UI.
- `just ci` / `just test` / coverage not run (heavy gates are pre-push/PR;
  per policy the reviewer/pusher should rely on the pre-push hook).
