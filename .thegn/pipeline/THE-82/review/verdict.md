# THE-82 security/test/bug review — verdict

PASS

Branch `tg/the-82-panel-help-coverage`, reviewed at `d3fef460` (after merging
`main` in as `e9eb70a5` — clean, no conflicts; the merge brought THE-79's
podman seam and touched none of this lane's files: `docs/help/panel.md`,
`crates/thegn-host/src/help/ratchet_tests.rs`, `test/help-panel-prose-ratchet.txt`
are byte-identical pre/post merge).

## What this lane is

Docs + one new ratchet test. Attack surface is accordingly narrow: a markdown
page (`include_str!`-embedded), a test module, a header-only allowlist file, a
justfile line, CLAUDE.md prose. No render-path, no `#[cfg]`, no color/glyph
literals, no deps, no async traits, no ignored `Result`s introduced. Architecture
ratchets: nothing touched. God-file guidance: `ratchet_tests.rs` is a test module,
not on the shrink-only list.

## Addenda checklist — each item proven, not taken on faith

1. **Merge main first** — done (`e9eb70a5`, clean). Full `git diff main...HEAD`
   reviewed: 11 files, exactly the lane's scope, nothing else.
2. **Test enumerates from the real source of truth** — confirmed:
   `every_panel_section_is_written_in_the_panel_page_prose` iterates
   `crate::help::context::vocabulary()` filtered to `panel:` keys
   (`context.rs` builds it from `SECTION_ORDER` chained with
   `Section::Debug`/`Section::Db`). A new section auto-fails the ratchet; there
   is no hand-maintained list to rot.
3. **Fails on a deliberately blanked entry (prove it, revert)** — done by this
   reviewer (the coder was barred from touching chunk-1's file and proved an
   equivalent arm instead; the addenda asked for the real thing):
   - Blanked every `usage` mention from `docs/help/panel.md` (including the
     `AI-usage`/`[[ai-usage]]` occurrences — see the boundary note below) with
     valid links, ran the test → **failed with exactly
     `panel section(s) with no written entry in docs/help/panel.md: ["panel:usage"]`**
     (the silent branch, real source-of-truth enumeration). Reverted byte-exact
     (`cmp` clean), test green again.
   - Appended `panel:usage` to the allowlist → **failed with exactly
     `panel section(s) now written but still allowlisted: ["panel:usage"]`**
     (the shrink-only branch). Restored byte-exact.
4. **Allowlist empty or justified per line** — `test/help-panel-prose-ratchet.txt`
   is header-only (4 comment lines, zero entries). Nothing to justify.
5. **Mandated gates** — all green on the final committed state:
   - `cargo nextest run -p thegn-host -E 'test(help) | test(complete) |
test(catalog_tests) | test(mq_assets)'` → **95/95 passed** (run twice: post-merge
     and post-fix).
   - `bash test/brand-guard.sh` → clean (run three times).
   - `cargo clippy -p thegn-host --tests --all-features` → clean (closes the
     chunk-2 "Unverified": dedicated test-target clippy pass).
   - `just quick thegn-host` → clean.
   - `nix develop -c treefmt --no-cache` over the tree → **0 changed** (the
     include_str! page, justfile, and CLAUDE.md are all formatter-clean).
   - Updater twin at merged HEAD: `THEGN_HELP_RATCHET_UPDATE=1 …
help_panel_prose_ratchet_update -- --ignored` regenerates the seeded file
     **byte-identical** (git status clean after run).
6. **Frame-affecting changes / e2e** — no re-record needed, verified two ways:
   no muse spec renders `panel.md`'s body (grep over `test/muse/specs/` for the
   new content finds nothing; `06-panel-system` shows the static docked help
   index, and page _titles_ — all unchanged — are what indexes show), and the
   ratchet/test/justfile/CLAUDE.md files are invisible to the compositor.
   The existing baselines' staleness on `main` is pre-existing per CLAUDE.md.

## Fact-check of the new prose (spot-checked against code, not the architect's word)

- `[pr_queue] enabled` default **false** (`config_pr_queue.rs:140`) — panel.md's
  "Off by default; turn it on with `[pr_queue] enabled = true`" is correct.
- `UsageConfig::default().enabled = true` (`config.rs:1584`) and
  `panel/mod.rs:364` hides the section only when explicitly disabled — the
  review-fix wording "On by default; `[usage] enabled = false` turns the feature
  off and hides this section" is correct.
- Git-family keys (`git stash show -p -u`, `/`·`↵`·`o` bat·`O` editor·`y` yazi,
  the `?` cheatsheet covering changes/commits/branches/stash — not files,
  `[git] structural_diff`) all match `docs/help/git-and-diffs.md`.
- `usage` table row sits between `media` and `keys`, matching `SECTION_ORDER`
  (Media → Usage → Keys).
- Help-tab keys match `docs/help/help.md` (`Tab`, `↑↓`/`j k`, `PgUp/PgDn`,
  `g`/`G`, `n`/`p`, `↵`, `/`, `Esc`; the `[`/`]` back/forward omission is fine —
  the section says "its keys are the overlay's" and links `[[help]]`).

## Code review of the new test

- **Matcher is memory-safe**: `body_mentions_panel_section` slices `hay` at
  `match_indices` offsets and `i + needle.len()`; all section keys are ASCII and
  `to_ascii_lowercase` preserves byte lengths/boundaries, so every slice index is
  a char boundary. No panic path. `&key["panel:".len()..]` is guarded by the
  `starts_with("panel:")` filter. Edge cases covered by the new
  `panel_section_keys_match_whole_words_only` unit test (string edges, empty
  haystack, case, hyphenated non-matches like `short-circuit`/`hardbound`).
- **Failure directions are correct**: a missing/unreadable allowlist reads as
  empty (`unwrap_or_default`), which can only make the test _fail_ on silent
  sections, never silently pass. Allowlist canonicality (sorted, dedup'd, live
  vocabulary, `panel:`-prefix) is asserted before use — a stray `zone:` line
  fails with a targeted message.
- **No swallowed errors on a primary path**: the updater's
  `fs::write(...).expect(...)` is the sanctioned updater pattern, `#[ignore]`d
  and env-gated (`THEGN_HELP_RATCHET_UPDATE=1`); it never runs in the gates.
- **No injection/path/permission surface**: the updater writes one fixed
  repo-relative path (`CARGO_MANIFEST_DIR/../../test/…`), no user input, no
  traversal, no shell-outs added.
- **No race in sanctioned flows**: the updater and the readers are separate
  nextest invocations run serially by `just help-ratchet-update`; the updater is
  skipped (ignored) in every default gate. (Theoretical only: hand-running the
  updater concurrently with the reader filter in one `nextest` invocation could
  interleave a file write with a read — not a reachable state via any documented
  command, non-blocking note.)

## Findings

### Fixed by review (committed `d3fef460`, `fix(the-82): justfile/CLAUDE.md ratchet-docs accuracy (review)`)

1. **justfile comment made stale by the branch**: the `help-ratchet-update`
   recipe comment enumerated two regenerated allowlists while the recipe now
   runs three updaters. Fixed: all three named; notes the frozen context file.
2. **CLAUDE.md over-claim extended by the branch**: "Four pinned-debt
   allowlists, all shrink-only and all regenerated by `just help-ratchet-update`"
   — false for `test/help-context-ratchet.txt`, which has no updater by design
   (seeded empty; the ratchet refuses additions). The imprecision predates the
   branch ("Three … all regenerated") but this branch rewrote the sentence, so it
   was corrected in the same breath: all shrink-only; three regenerated; the
   context allowlist frozen.

### Non-blocking observations

3. **Hyphenated compounds satisfy the mention floor**: `body_mentions_panel_section`
   treats `-` as a word boundary, so a body containing only "AI-usage" would
   count as writing about `usage`. This is the design's stated intent ("a floor
   against zero-mention claims, not a quality bar", same posture as the prose
   ratchet) and the current page mentions every key standalone; noting it so a
   future tighten-the-floor lane starts from facts, not a bug report.
4. **prq section renders "—" when the queue is off** (not filtered like
   media/usage) — product-level quirk already flagged by the architect review;
   not this lane.
5. **MSRV**: `is_none_or` (Rust 1.82+) — workspace `rust-version = "1.89"`, and
   the method is already used across thegn-host/thegn-core. No gate risk.

## Unverified items closed from the lane docs

- Chunk-1 "full help filter green" — closed (95/95 at merged HEAD).
- Chunk-2 "dedicated `clippy --tests` pass not run" — closed (clean, this review).
- Chunk-2 "e2e not run" — closed: no frame change, verified above.
- Heavy gates deferred per dev-loop policy — correct; pre-push remains the gate.

## Verdict

PASS — ready for the merge queue (`thegn integrate`; not run by this review).
