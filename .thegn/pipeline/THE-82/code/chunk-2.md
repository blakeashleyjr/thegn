# chunk-2 — ratchet: every panel section must be written in panel.md's prose

THE-82. Design: `.thegn/pipeline/THE-82/architect/design.md` (§4.1 + §4.3 is this chunk).
**Runs AFTER chunk-1** — the new test is seeded with an empty allowlist and expects the
`usage` entry to already exist in `docs/help/panel.md` (the only section never mentioned in
its body today).

## Files touched (exact paths)

- `crates/thegn-host/src/help/ratchet_tests.rs` — new test + helper + updater + module-doc
  touch-up.
- `test/help-panel-prose-ratchet.txt` — NEW, seeded empty (header comment only).
- `justfile` — `help-ratchet-update` recipe gains one line.
- `CLAUDE.md` — help-ratchet paragraph: "Three pinned-debt allowlists" → four.

No changes to `docs/help/` (that is chunk-1's file).

## Approach

Add a third member to the help ratchet family in
`crates/thegn-host/src/help/ratchet_tests.rs`, immediately after
`every_panel_context_has_a_documentation_page` (line 122). Copy the existing mechanics
verbatim where possible so the review is pattern-matching.

1. New path fn next to `context_ratchet_path()`:
   ```rust
   fn panel_prose_ratchet_path() -> PathBuf {
       PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../test/help-panel-prose-ratchet.txt")
   }
   ```
2. New helper (near `body_mentions`, line 267). Whole-word, case-insensitive:
   ```rust
   /// Whole-word mention of a panel section key in a page body. Not the prose
   /// ratchet's substring rule — keys like `pr`/`ci`/`db` would match inside
   /// unrelated words. `Section::as_key()` is the section's own label, so there
   /// is no chord/label fallback to add.
   fn body_mentions_panel_section(body: &str, key: &str) -> bool {
       let hay = body.to_ascii_lowercase();
       let needle = key.to_ascii_lowercase();
       hay.match_indices(&needle).any(|(i, _)| {
           hay[..i].chars().next_back().is_none_or(|c| !c.is_ascii_alphanumeric())
               && hay[i + needle.len()..].chars().next().is_none_or(|c| !c.is_ascii_alphanumeric())
       })
   }
   ```
3. The test (name it exactly `every_panel_section_is_written_in_the_panel_page_prose`):
   - `let reg = registry();` then
     `let page = reg.page("panel").expect("panel overview page ships in SOURCES");`
     (`page.body` is the parsed body — frontmatter excluded, same as the prose ratchet).
   - Read the allowlist with the existing `read_allowlist`; assert sorted + duplicate-free
     and that every entry is a live vocabulary key (mirror ratchet_tests.rs:136-142,
     substituting `crate::help::context::vocabulary()` and `panel:` prefixes).
   - Iterate `vocabulary()` filtered to `starts_with("panel:")`; for each
     `section = &key["panel:".len()..]`:
     - not mentioned and not allowlisted → collect into `silent`;
     - mentioned and allowlisted → collect into `now_written`.
   - Assert both empty with the family's shrink-only error texts (adapt
     ratchet_tests.rs:152-162 wording: "panel section(s) with no written entry in
     docs/help/panel.md — add the entry (its key must appear in the page body)"; and
     "now written but still allowlisted — delete those lines to lock in the win").
4. Updater twin (after `help_prose_ratchet_update`, line 322), same skeleton:
   ```rust
   #[test]
   #[ignore = "writes test/help-panel-prose-ratchet.txt; run via `just help-ratchet-update`"]
   fn help_panel_prose_ratchet_update() {
       if std::env::var("THEGN_HELP_RATCHET_UPDATE").as_deref() != Ok("1") { return; }
       // header comment lines + BTreeSet of silent "panel:<key>" entries;
       // std::fs::write(panel_prose_ratchet_path(), …).expect(…) — the sanctioned pattern.
   }
   ```
5. New file `test/help-panel-prose-ratchet.txt`, header comment only (seeded EMPTY):
   ```
   # help-panel-prose-ratchet — `panel:<key>` context keys with no written entry in
   # docs/help/panel.md (the key never appears in the page body). The context ratchet
   # guarantees reachability; this one guarantees coverage. This list may only SHRINK:
   # write the entry, delete the line (or run `just help-ratchet-update`).
   ```
6. `justfile` `help-ratchet-update` (line 236) gains:
   `THEGN_HELP_RATCHET_UPDATE=1 cargo test -p thegn-host help_panel_prose_ratchet_update -- --ignored`
7. `CLAUDE.md` — the help-ratchet paragraph: "Three pinned-debt allowlists" →
   "Four pinned-debt allowlists", adding "`test/help-panel-prose-ratchet.txt` (unwritten
   panel sections)". Update the enclosing sentence exactly, keep the rest of the prose.

Module doc: extend the `//!` header (lines 1-9) with one line naming the new member, e.g.
"and a third ratchet requires the panel overview page to _write about_ every panel section
(reachability ≠ coverage)."

## Overlap / dependency

File-disjoint from chunk-1 (different paths), but **serial — chunk-2 after chunk-1**: the
allowlist is seeded empty, so the test is red until chunk-1's `usage` entry lands. If this
chunk is built first, its scoped tests fail on `panel:usage` — that is the dependency, not a
bug.

## Tests to run (scoped)

- `cargo nextest run -p thegn-host help` — the issue's gate; selects everything under
  `help::`, incl. the claim ratchet, the prose ratchet, and the new test (nextest matches
  the full test path).
- `cargo nextest run -p thegn-host help::ratchet_tests` — the family in isolation; then
  `TheGN… click through nothing heavy`: no full-workspace or e2e runs.
- `just quick thegn-host` — clippy on the crate (watch clippy's
  `unnecessary_map_or`-era lints; `is_none_or` is stable since Rust 1.82, the toolchain here
  is 1.97.x, so use it directly).
- Pre-commit hooks (treefmt/shellcheck/yamllint) must pass on the staged files.

## Done criteria

- `cargo nextest run -p thegn-host help` fully green with the allowlist file present and
  containing ONLY its header comments (seeded empty).
- A meta-check that the test has teeth: temporarily removing the `usage` entry's `###`
  heading from `docs/help/panel.md` makes the new test fail on `panel:usage` (verify, then
  revert — do NOT land the removal).
- The allowlist is empty-but-shrink-only: the updater regenerates it without content when
  run via `just help-ratchet-update`.
- Commit with the EXACT subject: `test(the-82): panel-prose ratchet — every section written in docs/help/panel.md`
