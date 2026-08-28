# chunk-1 — docs: every panel section gets a written entry in docs/help/panel.md

THE-82. Design: `.thegn/pipeline/THE-82/architect/design.md` (§4.2 is this chunk).
**Must land before chunk-2** (the ratchet's empty-seeded allowlist requires the `usage`
entry to exist).

## Files touched (exact paths)

- `docs/help/panel.md` — the ONLY file. Do not touch frontmatter (`contexts:` claims are
  already correct post-THE-77). No Rust files, no test files.

## Approach

Four prose additions, so every one of the 30 panel sections has a written entry — what it
shows, its keys, its config. Copy facts from the linked pages; do not re-derive keys.

1. **New `## The git tab, section by section`** — insert before `## The work tab, section
by section` (currently line 127). The git tab is the only one with no section-by-section
   prose. One `###` per section, sourced from `docs/help/git-and-diffs.md` bullets
   (lines 12-34):
   - `### changes` — the working diff: `↵` inlines a file's hunks (binary/mode-only say so),
     `space` stages, `e` widens to the full git frame where `↵` drills into staging
     (line-level stage/unstage). Config: `[git] structural_diff` → `[[git-and-diffs]]`.
   - `### commits` — branch history; `E` edits (interactive rebase stopping at the commit);
     bare `e` stays the panel width cycle. → `[[git-and-diffs]]`.
   - `### branches` — local branches (+ open-PR badges on rows): check out, create, delete;
     wide frame shows the selected branch's recent commits. → `[[git-and-diffs]]`.
   - `### stash` — stash list: apply, pop, drop; `↵` shows the real diff
     (`git stash show -p -u`, untracked included). → `[[git-and-diffs]]`.
   - `### files` — the worktree tree: `/` filters (directories stay while any descendant
     matches), `↵` previews inline, `o` pages in bat, `O` opens the editor, `y` reveals in
     yazi. → `[[git-and-diffs]]`.
     Close the section with one line noting `?` shows each git-family section's own cheatsheet
     (changes/commits/branches/stash — not files) — consistent with the `## Keys` bullet.
2. **`### prq — the PR queue`** — insert after `### merge — the local merge queue` (before
   `### issues`). Shows the queued pull requests on the forge and what blocks them. Keys
   (verbatim from docs/help/pr-queue.md:117-119): `a` add · `x` remove · `r` re-watch a
   settled row · `c` clear · `D` refresh now · `o` open in a browser. Config `[pr_queue]`;
   CLI `thegn pr queue …`; link `[[pr-queue]]`.
3. **`### usage`** — in `## The system tab, section by section` (add after the
   `### share, forward, media, telemetry, keys` block, before `### db and debug`). The
   docked twin of the `Alt-u` AI-usage overlay: accounts worst-first (nearest a limit on
   top), width ladder (resting = one row per account with its worst limit; half = every
   limit; full = identity facts line + legend), `r` re-gathers now instead of waiting out
   the poll. Hidden unless `[usage] enabled`; thresholds `[usage] warn_percent` /
   `crit_percent` colour it with the gauge and overlay. Facts: docs/help/ai-usage.md.
   Link `[[ai-usage]]`. ALSO: add a `| `usage`  | AI-account rate-limit usage — see
[[ai-usage]] |` row to the system table (`## What each section shows`) and add `usage`
   to the **system** intro bullet (line ~24), which currently omit it.
4. **`## The help tab`** — short entry after the system tab's prose: the docked twin of the
   `F1` overlay (see the intro bullets, line ~26); keys are the help overlay's: `Tab`
   switches contents tree/page, `↑↓`/`j k` move, `PgUp/PgDn`, `g`/`G`, `n`/`p` cycle links,
   `↵` follows, `/` searches, `Esc` closes (docs/help/help.md:9-20). No config. Link
   `[[help]]`.

Style: match the existing section entries (bold lead word or `### <key> — <label>` heading,
chords in backticks, `[[page]]` links). Keep the "shows / keys / config" triple visible in
each entry. Let treefmt/prettier re-align the pipe tables (the pre-commit hook does it; do
not hand-pad — run the commit and re-`git add` if the hook rewrites, as normal).

## Overlap / dependency

File-disjoint from chunk-2, but **serial**: chunk-2's ratchet test reads this page and is
seeded with an empty allowlist, so `panel:usage` (the one key never mentioned today) must be
written here first. Land this chunk, then chunk-2.

## Tests to run (scoped)

- `just quick thegn-host` — the page is `include_str!`'d; a broken frontmatter/table would
  show up in the host crate's clippy/test compile.
- `cargo nextest run -p thegn-host help` — page validation + `context_pages_resolve` stay
  green (no frontmatter changes, so claims are untouched).
- Self-check for the ratchet that lands in chunk-2 (every section key must appear as a
  whole word in the page body, case-insensitive): after the edit, all 30 keys — changes,
  commits, branches, stash, files, mine, across, pr, ci, merge, prq, issues, problems, jobs,
  tests, symbols, notifications, logs, sandbox, hosts, environments, share, forward,
  telemetry, media, usage, keys, help, debug, db — pass; `usage` was the only failure before.

## Done criteria

- `docs/help/panel.md` has a written entry (shows + keys + config) for every one of the 30
  sections; `usage` mentioned in body prose, the system table, and the intro bullet; git tab
  has `###` entries; prq and help upgraded from table-row/bullet to real entries.
- No frontmatter changes; `cargo nextest run -p thegn-host help` green; pre-commit
  (treefmt/shellcheck/yamllint) clean.
- Commit with the EXACT subject: `docs(the-82): every panel section has a written entry in docs/help/panel.md`
