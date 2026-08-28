# THE-82 architect review — verdict

**APPROVED** — no revision chunk required. Two small corrections applied by the
architect and committed as `27ecc655`
(`docs(the-82): usage section is on by default; allowlist entries must be panel: keys (review)`).

Branch `tg/the-82-panel-help-coverage`, reviewed at `27ecc655` (after merging
`main` in as `3cafddc9` — clean, no conflicts; the merged tree's section
registry is unchanged: 28 live sections + `Debug`/`Db`, `Usage` between `Media`
and `Keys`, so the design's 30-key enumeration still holds post-THE-70/THE-83).

## Mandated gate

`cargo nextest run -p thegn-host -E 'test(help) | test(complete) |
test(catalog_tests) | test(mq_assets)'` — **95/95 PASS**, run twice: on the
merged pre-fix tree and again after the review fixes. The new ratchet
(`every_panel_section_is_written_in_the_panel_page_prose`) and its matcher test
confirmed present and passing in the run.

## Chunk-1 (docs) — verified

- All 30 `panel:*` keys are written in `docs/help/panel.md`'s body; frontmatter
  untouched. Git tab gained its section-by-section block; `prq` and `help`
  upgraded from table-row/bullet to real entries; `usage` added to intro bullet,
  system table (after `media`, matching `SECTION_ORDER`), and prose.
- Fact-check against sources, all faithful: git family entries match
  `git-and-diffs.md` (↵/space/e/E, `git stash show -p -u`, files `/`·`o`·`O`·`y`,
  the `?` cheatsheet scope changes/commits/branches/stash-not-files,
  `[git] structural_diff`); prq keys verbatim from `pr-queue.md`, and
  `[pr_queue] enabled` default **false** confirmed (`config_pr_queue.rs:140`);
  usage entry matches `ai-usage.md` (Alt-u twin, worst-first, width ladder,
  `r` re-gather, warn/crit thresholds); help tab keys match `help.md`.
- **Fixed in review:** the usage entry said "Hidden unless `[usage] enabled`" —
  but `[usage] enabled` defaults to **true** (`config.rs`, `UsageConfig::default`)
  and the section hides only on explicit `enabled = false`
  (`panel/mod.rs:364-367`). Under prq's "Off by default; turn it on with…" it
  read as opt-in. Now states the actual default. (The phrasing originated in the
  design's own §4.2 — the coder copied the architect's error faithfully.)

## Chunk-2 (ratchet) — verified

- Matches design §4.1: enumeration from `context::vocabulary()` (a new section
  auto-fails — no hand list to rot), whole-word case-insensitive matcher with
  ASCII-alnum boundaries and `is_none_or`, sorted/duplicate-free/live-key
  allowlist asserts, shrink-only error texts, `#[ignore]` updater twin gated on
  `THEGN_HELP_RATCHET_UPDATE=1`, justfile line, CLAUDE.md "Four allowlists",
  module-doc line. Allowlist seeded header-only.
- Updater: ran it myself — regenerates the seeded file **byte-identical**
  (`cmp` clean). Empty-but-shrink-only invariant holds.
- Teeth: verified independently. Now-written branch: appending `panel:usage` to
  the allowlist fails with exactly the shrink-only message; restored byte-exact.
  Silent branch: the matcher unit test plus the prefix check (below). Chunk-2's
  deviation from the spec's meta-check method was correct — removing only the
  `### usage` heading could never have flipped a body-mention floor, and the
  Lead's hands-off rule on chunk-1's file justified the substitute.
- **Fixed in review:** chunk-2-done claimed "a `panel:`-prefix assertion so a
  stray `zone:`/`overlay:` line can't hide" — no such assertion existed (the
  code only asserted live-vocabulary, mirroring the context ratchet). Design
  §4.1 step 3 required "a live `panel:*` vocabulary key", so the check is now
  in `every_panel_section_is_written_in_the_panel_page_prose`; verified
  `zone:sidebar` fails with the prefix message. Note for future lanes: done
  reports must not describe code that isn't there — this one cost a review fix.

## Unverified items closed

- Chunk-1: full `help` filter green — **now true** (was blocked only by
  chunk-2's seeded `panel:usage` line, since removed; 95/95 observed).
- Chunk-1 e2e: **no muse spec renders panel.md's body** — `06-panel-system`
  shows the docked help index ("whose text is static"), `27` only regex-matches
  the overlay border `╭─ help` then closes. No frame change; baselines are
  already stale on `main` per CLAUDE.md. No re-record needed.
- Both chunks' heavy-gate deferrals: correct per the dev-loop policy; the
  mandated scoped gate stands in for this lane.

## Non-blocking observations

- The prq section row renders "—" (not hidden) when the queue is off, unlike
  media/usage which `resolve_order` filters. `pr-queue.md` describes the
  feature the same way, so panel.md is not wrong; a product-level quirk for a
  future lane, not this one.
- `chunk-1.md`/`chunk-2.md` and the design's §4.2 "Hidden unless" wording should
  be read as superseded by `27ecc655`.

## Revision chunks

None — APPROVED as-is with `27ecc655` applied.
