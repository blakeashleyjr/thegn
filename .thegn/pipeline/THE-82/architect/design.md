# THE-82 — Grow docs/help/panel.md to cover every panel section (reachability ≠ coverage)

Architect design. Branch `tg/the-82-panel-help-coverage`. Evidence is file:line in this
worktree at design time (HEAD `d361b60a`).

## 1. Problem

THE-77 emptied `test/help-context-ratchet.txt`: every `panel:<section>` context key is now
**claimed** by some page's `contexts:` frontmatter, so pressing F1 in any section lands on a
real page (`crates/thegn-host/src/help/pages.rs:187-205` — `context_pages_resolve`, whose
comment "Sections with no dedicated page fall back to the panel overview" is exactly the
reachability guarantee the issue cites). But reachability is not coverage: the fallback
surface itself — `docs/help/panel.md` — does not describe every section. Measured gap below.

## 2. Source of truth for "every panel section"

The section registry, not the help corpus, is authoritative:

- `crates/thegn-host/src/panel/mod.rs:112` — `pub enum Section` — 30 variants: the 28 live
  accordion sections plus the two reserved placeholders `Debug` / `Db` (dead variants,
  `#[allow(dead_code)]`).
- `crates/thegn-host/src/panel/mod.rs:186` — `pub const SECTION_ORDER: [Section; 28]` —
  the built-in display order (Git 5, Work 11, System 11, Help 1).
- `crates/thegn-host/src/panel/mod.rs:315` — `Section::as_key()` → `label()`: the stable
  config/persistence key (`changes`, `merge`, `prq`, …). These keys ARE the vocabulary keys.
- `crates/thegn-host/src/help/context.rs:43-60` — `vocabulary()` builds `panel:<as_key()>`
  for `SECTION_ORDER` **chained with `[Section::Debug, Section::Db]`**. This is the same
  enumeration the existing context ratchet iterates (`ratchet_tests.rs:122`), so reusing it
  makes a NEW section auto-fail the new ratchet — no hand-maintained list to rot.

The 30 keys: changes, commits, branches, stash, files, mine, across, pr, ci, merge, prq,
issues, problems, jobs, tests, symbols, notifications, logs, sandbox, hosts, environments,
share, forward, telemetry, media, usage, keys, help, debug, db.

## 3. Current-state audit (measured, not hypothesised)

Claims (THE-77, all 30 keys claimed — grep over `docs/help/*.md` frontmatter): git-family →
`git-and-diffs.md`; merge → `merge-queue.md`; prq → `pr-queue.md`; pr+ci → `review-a-pr.md`;
sandbox+environments → `sandboxing.md`; share+forward → `share-and-forward.md`; media →
`media.md`; usage → `ai-usage.md`; help → `help.md`; the remaining 13 + `zone:panel` →
`panel.md` itself (docs/help/panel.md:7-21).

Panel.md **body** coverage, checked with a case-insensitive whole-word match of each key
(frontmatter stripped, mirroring the parsed `page.body` the ratchet sees):

| Status                                             | Keys                                                                                                                                                                      |
| -------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Mentioned + real prose entry (shows/keys/config)   | mine, across, pr, ci, merge, issues, problems, jobs, tests, symbols, notifications, logs, sandbox, hosts, environments, share, forward, media, telemetry, keys, debug, db |
| Mentioned but entry is thin (no keys/config prose) | changes, commits, branches, stash, files (table rows only, docs/help/panel.md:64-72), prq (table row only, :82), help (intro bullet only, :26)                            |
| **Never mentioned at all**                         | **usage** (absent from the intro list, the system table, and the section-by-section prose)                                                                                |

So the ratchet has exactly one red key today (`panel:usage`) and the docs work is: give the
git tab its section-by-section entries, upgrade prq and help to real entries, and add usage
everywhere it belongs.

## 4. Design

### 4.1 The test — one new member of the help ratchet family

In `crates/thegn-host/src/help/ratchet_tests.rs`, after
`every_panel_context_has_a_documentation_page` (line 122):

```rust
/// Claim (the context ratchet above) is reachability; this is coverage: the
/// panel overview page must actually *write about* every panel section —
/// `SECTION_ORDER` + the two reserved placeholders, enumerated from the same
/// `context::vocabulary()` the claim ratchet uses.
#[test]
fn every_panel_section_is_written_in_the_panel_page_prose() { … }
```

Mechanics, copied from the two existing ratchets so review is pattern-matching:

1. `let reg = registry();` (same clean-validation gate, ratchet_tests.rs:65).
2. `let page = reg.page("panel").expect("panel overview page ships in SOURCES");`
3. Allowlist `test/help-panel-prose-ratchet.txt` (new file, **seeded empty** — header
   comment only), read via the existing `read_allowlist`; assert sorted + duplicate-free and
   that every entry is a live `panel:*` vocabulary key (staleness check, as in the context
   ratchet at ratchet_tests.rs:136-142).
4. For each `key` in `crate::help::context::vocabulary()` where `key.starts_with("panel:")`:
   `section = &key["panel:".len()..]`; if NOT mentioned in `page.body` and not allowlisted →
   fail with the shrink-only error text; if mentioned but allowlisted → fail with the
   "delete the line to lock in the win" text.
5. Updater twin `help_panel_prose_ratchet_update` (`#[ignore]`, gated on
   `THEGN_HELP_RATCHET_UPDATE=1`), writing the file — same shape as `help_prose_ratchet_update`
   (ratchet_tests.rs:322).

Mention rule — `body_mentions_section(body, key) -> bool`: case-insensitive **whole-word**
containment (non-ASCII-alphanumeric on both sides):

```rust
fn body_mentions_section(body: &str, key: &str) -> bool {
    let hay = body.to_ascii_lowercase();
    let needle = key.to_ascii_lowercase();
    hay.match_indices(&needle).any(|(i, _)| {
        hay[..i].chars().next_back().is_none_or(|c| !c.is_ascii_alphanumeric())
            && hay[i + needle.len()..].chars().next().is_none_or(|c| !c.is_ascii_alphanumeric())
    })
}
```

Why word-boundary and not the prose ratchet's substring: `body_mentions`
(ratchet_tests.rs:267) matches long action ids where substring noise is harmless; section
keys include `pr`, `ci`, `db` — substring would make the test vacuous for them. Why no
chord/label fallback: `Section::as_key()` **is** the label (`panel/mod.rs:315`), and every
rendered mention of a section in the corpus is already its key — the rule is a floor against
zero-mention claims, not a quality bar (same posture as the prose ratchet's doc comment).

Deliberate scope cut: the test targets the `panel` page only, **not** a generic "page
claiming `panel:<key>` must mention `<key>`". Sections with dedicated pages are covered
there, and a generic rule would trip on pages that legitimately never repeat the bare key
(e.g. `pr-queue.md` needn't write the literal word "prq"). The issue's lane is panel.md's
own coverage; the generic variant is a possible follow-up, not part of this change.

### 4.2 The docs — every section gets a written entry (shows / keys / config)

`docs/help/panel.md` grows, prose only — **no frontmatter changes** (claims are already
correct post-THE-77):

1. New `## The git tab, section by section` before the work tab's (currently the git tab is
   the only one with no prose): `### changes` / `### commits` / `### branches` / `### stash`
   / `### files`. Facts sourced from `docs/help/git-and-diffs.md:12-34` (per-section keys:
   `↵` inline hunks, `space` stage, `e` widen to the git frame with staging drill-in, `E`
   edit/reword in commits, open-PR badges on branches, stash's real diff via
   `git stash show -p -u`, files' `/` filter + `o` bat + `O` editor + `y` yazi) — the coder
   copies from that page rather than re-deriving, and links `[[git-and-diffs]]`. Config
   pointer: `[git] structural_diff`.
2. `### prq — the PR queue` after `### merge`: queued PRs + blockers, keys `a` add · `x`
   remove · `r` re-watch · `c` clear · `D` refresh now · `o` browser (docs/help/pr-queue.md:117-119),
   `[pr_queue]` config, link `[[pr-queue]]`.
3. `### usage` in the system tab's section-by-section (and a `usage` row in the system table
   - the intro bullet, which currently omit it): docked twin of the `Alt-u` overlay,
     worst-first per account, width ladder (resting = worst limit per account, half = every
     limit, full = identity facts + legend), `r` re-gathers now; hidden unless
     `[usage] enabled`; thresholds `[usage] warn_percent` / `crit_percent`
     (docs/help/ai-usage.md). Link `[[ai-usage]]`.
4. `## The help tab` short entry for `help`: the docked twin of the F1 overlay; keys are the
   overlay's (`Tab` tree/page, `n`/`p` links, `↵` follow, `/` search, `Esc` closes —
   docs/help/help.md:9-20); no config. Link `[[help]]`.

treefmt/prettier re-aligns the edited pipe tables (pre-commit hook handles it; coder should
let the hook or `nix fmt` normalise rather than hand-padding).

### 4.3 Wiring

- `justfile` — `help-ratchet-update` recipe (justfile:236-239) gains:
  `THEGN_HELP_RATCHET_UPDATE=1 cargo test -p thegn-host help_panel_prose_ratchet_update -- --ignored`.
- `CLAUDE.md` — the help-ratchet paragraph says "**Three** pinned-debt allowlists"; becomes
  four, adding `test/help-panel-prose-ratchet.txt` (unwritten panel sections).
- New `test/help-panel-prose-ratchet.txt`, header comment only (seeded empty):
  format = one `panel:<key>` line per pinned debt, same conventions as
  `test/help-context-ratchet.txt`.

### 4.4 Invariants / ratchets respected

- Test-only + docs + build-file change: no render-path touch (render_plan untouched), no new
  deps, no `#[cfg]`, no color/glyph literals, no async trait methods, no ignored `Result`s
  (the updater's `std::fs::write(…).expect()` is the already-sanctioned pattern used by both
  existing updaters).
- The test id lives under `help::ratchet_tests::…`, so the issue's gate
  `cargo nextest run -p thegn-host help` selects it (nextest matches on the full test path).
- Section renames: `as_key()` changes ⇒ vocabulary changes ⇒ the old allowlist entry becomes
  stale (validity assert refuses it) and the new key needs prose — the same
  ratchet-beats-drift behaviour as the context ratchet. The `from_key` back-compat aliases
  (`"prs"|"git" → pr`, `"tasks" → jobs`, panel/mod.rs:319-326) are config aliases, not
  vocabulary keys; out of scope.
- `debug`/`db` stay documented as reserved placeholders (panel.md:258-266 already does;
  vocabulary includes them via context.rs:57).

## 5. Alternatives considered

- **Generic claim+must-mention for `panel:*` across all pages** — rejected for this lane:
  changes the contract of five other pages, risks false positives (`prq`), and the issue
  names panel.md. Documented as a possible follow-up.
- **Hand-written list of sections in the test** — rejected: duplicates `vocabulary()` and
  silently rots when a section is added; vocabulary() is the enumeration the claim ratchet
  already trusts.
- **Backtick-required mention (`` `prq` ``)** — stricter, but the placeholders are written
  bold (`**db**`), not backticked, and the floor should not force formatting churn.

## 6. Chunk plan (2 chunks, file-disjoint, SERIAL — chunk 2 depends on chunk 1)

- **chunk-1** — docs: `docs/help/panel.md` only. Adds the written entries (§4.2). Must land
  first: the ratchet's empty-seeded allowlist (§4.1 step 3) requires `usage` to be mentioned.
- **chunk-2** — ratchet: `crates/thegn-host/src/help/ratchet_tests.rs` +
  `test/help-panel-prose-ratchet.txt` (new) + `justfile` + `CLAUDE.md`. Lands after chunk-1;
  its green test is also the acceptance check on chunk-1's coverage.

Each chunk file under `.thegn/pipeline/THE-82/code/` is self-contained (files, approach,
scoped tests, done-criteria incl. exact commit subject).
