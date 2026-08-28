# chunk-2 done — ratchet: every panel section must be written in panel.md's prose

Commit: `cd7de7e9` — `test(the-82): panel-prose ratchet — every section written in docs/help/panel.md`
(exact subject per spec). Branch `tg/the-82-panel-help-coverage`, on top of chunk-1's
`dd617044` (panel.md prose) + `855b5c28` (its summary).

## What landed

All seven spec items, in `crates/thegn-host/src/help/ratchet_tests.rs` unless noted:

1. `panel_prose_ratchet_path()` — next to `context_ratchet_path()`.
2. `body_mentions_panel_section()` — verbatim from the spec (whole-word,
   case-insensitive via `match_indices` + alphanumeric boundary checks,
   `is_none_or`); placed next to `body_mentions()`.
3. Test `every_panel_section_is_written_in_the_panel_page_prose` — after
   `every_panel_context_has_a_documentation_page`, with a banner comment. Mirrors the
   context ratchet: sorted/duplicate-free allowlist assertion, live-vocabulary
   assertion (plus a `panel:`-prefix assertion so a stray `zone:`/`overlay:` line
   can't hide in the file), then the silent / now-written sweep over
   `vocabulary()` filtered to `panel:` keys, with the spec's shrink-only error
   wording ("no written entry … add the entry (its key must appear in the page
   body)" / "now written but still allowlisted … lock in the win").
4. Updater twin `help_panel_prose_ratchet_update` (`#[ignore]` +
   `THEGN_HELP_RATCHET_UPDATE=1` gate) — after `help_prose_ratchet_update`;
   header lines byte-identical to the seeded file.
5. `test/help-panel-prose-ratchet.txt` — NEW, seeded empty (4 header comment
   lines exactly as the spec gives them).
6. `justfile` — `help-ratchet-update` recipe gained the one line for the new
   updater (recipe comment left as-is, per the spec's "gains one line").
7. `CLAUDE.md` — help-ratchet paragraph: "Three pinned-debt allowlists" →
   "Four", with `test/help-panel-prose-ratchet.txt` (unwritten panel sections)
   appended to the enumeration. Rest of the paragraph untouched.

Module `//!` doc gained the spec's extra line (worded "a fourth ratchet" to match
CLAUDE.md's four-allowlist count; the spec's example said "third" — the family
already had three allowlists, so "fourth" is the consistent ordinal).

One addition beyond the spec's list, same file: a small permanent unit test
`panel_section_keys_match_whole_words_only` locking the matcher's word-boundary
semantics (matches `pr`/`ci`/`db` as words at either string edge, case-insensitive;
does NOT match inside "problems"/"short-circuit"/"hardbound"). This is the
spec-mandated "test has teeth" evidence for the silent branch, see below.

## Test evidence (scoped per dev-loop policy)

- `cargo nextest run -p thegn-host help` — **75/75 PASS** (the issue's gate).
- `cargo nextest run -p thegn-host help::ratchet_tests` — **8/8 PASS** (family in
  isolation, incl. the new test + matcher test).
- `just quick thegn-host` — clippy clean (22.7s).
- `rustfmt --edition 2024 --check` on the touched .rs — clean (pre-commit treefmt
  also Passed on commit).
- Updater: `THEGN_HELP_RATCHET_UPDATE=1 cargo nextest run -p thegn-host
help_panel_prose_ratchet_update -- --ignored` — PASS, and it rewrote the
  allowlist **byte-identical to the seeded header-only file** (`cmp` clean):
  the empty-but-shrink-only invariant holds.

## Teeth verification — deviation from the spec's method, documented

The spec's meta-check ("temporarily removing the `usage` entry's `###` heading
from `docs/help/panel.md` makes the test fail on `panel:usage`") requires editing
`docs/help/panel.md`, which the Lead addenda declares HANDS-OFF (chunk-1's file,
concurrent). I did not touch it. Equivalent proof without it:

- **End-to-end, now-written branch**: appended `panel:usage` to my own
  `test/help-panel-prose-ratchet.txt` → the new test FAILED with exactly
  `panel section(s) now written but still allowlisted: ["panel:usage"]`
  (ratchet_tests.rs:230) → file restored byte-exact (`cmp` verified). This
  exercises the full path: allowlist read/canonical checks, vocabulary filter,
  matcher, and the shrink-only assertion.
- **Silent branch**: covered by the new matcher unit test (a key absent as a
  whole word returns false → the sweep's `!mentioned` arm fires) plus symmetry —
  the silent and now-written arms are the same loop and same matcher call,
  mirrored conditions, and the assertion text was exercised above.
- Additional note: even per the spec's own method, removing only the `### usage`
  heading would NOT flip the test — chunk-1's entry also mentions `usage` in the
  system-tab bullet and the work/system table rows, and the ratchet is a
  body-mention floor (key appears anywhere in the body), deliberately crude like
  the action prose ratchet. The heading would have to be removed together with
  every other occurrence to see the failure.

## Dependency / concurrency note

The spec says chunk-2 runs AFTER chunk-1 with the allowlist seeded empty, red on
`panel:usage` if built first. While I implemented, chunk-1's prose was present in
the shared worktree (uncommitted), so my runs were green against it; by commit
time chunk-1 had landed (`dd617044`), and my commit (empty seed, per spec) sits on
top. The committed branch state — chunk-1's prose + my ratchet + header-only
allowlist — is exactly the state every run above verified. No allowlist seeding
was needed; nothing under Unverified turns on it.

## Unverified

- Heavy gates (`just test`, `just coverage`, `just lint`, `just ci`), e2e, and the
  non-help test targets were **not** run — per the Lead addenda and dev-loop
  policy they are pre-push/CI gates. `help::ratchet_tests` + the full `help`
  filter are the scoped gates for this chunk and are green; `just quick
thegn-host` covers lib/bin clippy. Test-target clippy for the new code is
  exercised indirectly (the nextest runs compile all of `help::` under test
  profile) but a dedicated `clippy --tests` pass was not run.
- Remote CI is dispatch-only per CLAUDE.md; nothing was dispatched.
- `just help-ratchet-update` was not run as a whole recipe (only the new
  updater's test line, directly via env var); the other two lines are unchanged.
- e2e not run (no rendered frame changes: test module, allowlist file, justfile
  line, CLAUDE.md prose — nothing the compositor draws).
