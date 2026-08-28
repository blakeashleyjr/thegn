# chunk-1 done — docs: every panel section has a written entry in docs/help/panel.md

**Commit:** `dd617044` — `docs(the-82): every panel section has a written entry in docs/help/panel.md`
**Branch:** `tg/the-82-panel-help-coverage` · **Files touched:** `docs/help/panel.md` only (+70/−2)

## What was done

Four prose additions so all 30 panel sections have a written shows/keys/config entry,
exactly per chunk spec §4.2 (design §4.2):

1. **`## The git tab, section by section`** inserted before the work tab's block —
   `### changes` (↵ inline hunks, binary/mode-only say so, `space` stage, `e` → full git
   frame with staging drill-in, `[git] structural_diff` config), `### commits` (`E`
   edit/reword, bare `e` stays width cycle), `### branches` (checkout/create/delete,
   open-PR badges, wide-frame selected-branch commits), `### stash` (apply/pop/drop, real
   diff via `git stash show -p -u`), `### files` (`/` filter, `↵` preview, `o` bat, `O`
   editor, `y` yazi) — all copied from `docs/help/git-and-diffs.md` bullets, each linking
   `[[git-and-diffs]]`; closed with the one-line `?` git-family cheatsheet note
   (changes/commits/branches/stash — not files), consistent with the `## Keys` bullet.
2. **`### prq — the PR queue`** after `### merge`, before `### issues` — keys verbatim
   from `docs/help/pr-queue.md` (`a` add · `x` remove · `r` re-watch a settled row ·
   `c` clear · `D` refresh now · `o` open in a browser), `[pr_queue] enabled = true`
   off-by-default note, CLI `thegn pr queue …`, link `[[pr-queue]]`.
3. **`### usage — AI-account rate-limit usage`** in the system tab's block, after the
   `share/forward/media/telemetry/keys` entry, before `### db and debug` — docked twin of
   the `Alt-u` overlay, worst-first ordering, width ladder (resting = worst limit per
   account / half = every limit / full = identity facts line + legend), `r` re-gather,
   hidden unless `[usage] enabled`, `warn_percent`/`crit_percent` thresholds; link
   `[[ai-usage]]`. Plus the `| \`usage\` | AI-account rate-limit usage — see [[ai-usage]] |`row in the system table (after`media`, matching the built-in rotation order in
`SECTION_ORDER`) and `usage` added to the **system** intro bullet.
4. **`## The help tab`** at the end of the page — docked twin of the `F1` overlay; keys
   copied from `docs/help/help.md` (`Tab`, `↑↓`/`j k`, `PgUp/PgDn`, `g`/`G`, `n`/`p`,
   `↵`, `/`, `Esc`); no config; link `[[help]]`.

Frontmatter untouched (`contexts:` claims unchanged, as required). Every code span kept on
a single source line (the repo's prettier shim is non-idempotent on spans wrapping across
line breaks).

## Verification

- **Ratchet self-check (chunk's own criterion): PASS** — all 30 section keys (changes,
  commits, branches, stash, files, mine, across, pr, ci, merge, prq, issues, problems,
  jobs, tests, symbols, notifications, logs, sandbox, hosts, environments, share,
  forward, telemetry, media, usage, keys, help, debug, db) match as whole words,
  case-insensitive, in the page body (frontmatter excluded). `usage` was the only failure
  before; now written in intro bullet + table row + prose, as the spec requires.
- **`just quick thegn-host`: PASS** (clippy on the host lib/bin — page is `include_str!`'d,
  so the page compiles into the binary).
- **prettier (repo's `prettier-stable` semantics): unchanged** — ran before commit;
  pre-commit `treefmt` hook also passed with no rewrite.
- **`cargo nextest run -p thegn-host help --no-fail-fast`: 74/75 PASS**, including
  `registry_validates_cleanly`, `full_shipped_pages_render_at_common_widths`,
  `page_action_claims_are_real_action_ids` — page validation and rendering are clean with
  the new prose.

## Known interaction with chunk-2 (the one failing test — by design)

The 1 failure is `help::ratchet_tests::every_panel_section_is_written_in_the_panel_page_prose`
— **chunk-2's new ratchet** (in flight concurrently in this worktree). It fails with:

> panel section(s) now written but still allowlisted: ["panel:usage"] — Delete those lines
> from test/help-panel-prose-ratchet.txt to lock in the win.

i.e. the test itself confirms chunk-1 succeeded: `panel:usage` is now written, so the
provisionally seeded `panel:usage` line in chunk-2's `test/help-panel-prose-ratchet.txt`
has become stale. Deleting that one line is the designed serial handoff, but that file is
chunk-2's territory (my spec: "docs/help/panel.md — the ONLY file. No Rust files, no test
files"), so I left it. Chunk-2 must delete the seeded `panel:usage` line (or rerun
`just help-ratchet-update`) when landing, after which the full `help` filter is green.

## Unverified

- **Full `cargo nextest run -p thegn-host help` green state** — green except the chunk-2
  seeded-allowlist line described above; cannot be green from this chunk alone without
  touching a file my spec forbids. Chunk-2's landing closes it.
- **e2e** — not run (per instructions). Prose-only change to an on-demand help page; no
  default-frame pixels change, so no `just e2e-update` is expected, but this was not
  verified against the snapshot suite.
- **Heavy gates** (`just test`, `just coverage`, `just ci`) — deliberately not run per the
  dev-loop policy / lead addendum; pre-push remains the gate.
- Chunk-2's concurrently-edited `crates/thegn-host/src/help/ratchet_tests.rs` was present
  in the worktree during the test run; its test code is theirs and was exercised as-found
  (the new test compiled and ran; only its seeded allowlist line failed, as described).
