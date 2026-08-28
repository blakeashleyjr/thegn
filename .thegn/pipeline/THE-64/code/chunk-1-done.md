# THE-64 — chunk 1 done: the `[ui] sidebar_dividers` config key

**Commit:** `7eaec5c8` — `feat(config): [ui] sidebar_dividers key (THE-64)`
**Branch:** `tg/the-64-sidebar-distinction`

## What landed

Exactly the chunk spec, exactly the two files:

- `crates/thegn-core/src/config_ui.rs`
  - `pub sidebar_dividers: bool` on `UiConfig`, directly after
    `sidebar_nav_skips_collapsed` (sidebar-behaviour keys stay together),
    with the spec's doc comment.
  - `sidebar_dividers: true` in `impl Default for UiConfig`, same position.
  - New unit test `sidebar_dividers_defaults_on_and_toggles` in the existing
    `mod tests`: empty `[ui]` table ⇒ `true` (default); explicit
    `sidebar_dividers = false` round-trips to `false`.
- `config/config.toml.example`
  - `sidebar_dividers = true  # blank separator row above each workspace
header (off = the old dense layout)` in the `[ui]` section, directly after
    `sidebar_nav_skips_collapsed = true`. This satisfies the
    `crates/thegn-core/tests/config_example.rs` drift test and auto-populates
    the generated in-app config-reference help page — no hand-written page.

## Verification (scoped only, per dev-loop policy)

- `just quick thegn-core` — clippy clean on the crate.
- `cargo nextest run -p thegn-core config_ui` — 9/9 passed (incl. the new test).
- `cargo nextest run -p thegn-core --test config_example` — 2/2 passed
  (`example_config_documents_every_section_and_key` confirms the example-file
  drift gate).
- `cargo fmt -p thegn-core` — no changes (pre-commit treefmt hook also ran on
  the commit).

## Invariants honored

- `config_validate.rs` **90** `config_enum` pin: untouched (bool adds no enum);
  diff is exactly the two files above, 19 insertions.
- No `test/*-ratchet.txt` entry added (shrink-only allowlists unchanged).
- No help-page edits needed — the config-reference page is generated from
  `config.toml.example`; no action id / keybind / zone / panel section added,
  so the help ratchets are unaffected.
- `thegn-core` 95% line coverage: the new field is covered by the round-trip
  test (per the chunk spec's coverage note).
- `sidebar_view.rs` untouched — chunk 2 owns the `SidebarDisplay` wiring.

## Unverified

- `just test` / `just ci` / coverage / e2e were not run (full-workspace gates
  are forbidden for this stage). The `config_example` drift test and clippy are
  the scoped gates that apply to these files; nothing else in the workspace
  consumes the new field yet, so no other target's compile is at risk.
- Coverage percentage not measured locally (instrumented recompile is a
  pre-push/CI gate); the spec states the round-trip test suffices for a plain
  bool with a `Default`.
