# Chunk 4 — done

**Issue:** THE-68. **Branch:** `tg/the-68-log-noise`. **Chunk:** 4 (specs, help
prose, changelog). No source files touched, so nothing here blocks or is blocked
by chunks 1–3 beyond wording.

## What landed

### 1. `openspec/changes/fix-attention-signal-noise/` — new change folder

Shape copied from `fix-land-merged-folder` (the most recent `fix-*` folder), per
`CLAUDE.md` § _Spec-driven development_ and `openspec/config.yaml`'s rules.

- **`proposal.md`** — `## Why` states both symptoms with their root causes (the
  OSC producer using an append-only log as a cross-process channel for live
  state, and the fail-open display / fail-closed clear asymmetry) and the three
  consequences that follow from the first. `## What Changes` carries the thesis
  ("an inbox row is an event you might otherwise miss; a raised hand is live
  state you can already see") and the seven concrete changes. `## Impact` cites
  the roadmap link `CLAUDE.md` requires — group **AI (420, 424, 426, 428)**
  (rules engine, per-event opt-in, DND, notification history/center) and group
  **S (256)** (needs-attention surfacing), noting **AQ (524)** reuses the same
  tier model — names all four implementation chunks with their files, lists the
  gates (schema 56 → 57, the config-key gates, the `db*.rs` coverage
  obligation), and records that `add-osc-attention-signaling` stays unarchived
  and unedited.
- **`design.md`** — condenses architect design §3–§5: the `session_attention`
  DDL, the full lifecycle table (seven arms, with the two load-bearing ones
  called out), why the demand reuses `AgentNeedsInput` rather than getting a new
  tier (the `stage_blocked_since` precedent, quoted), why the clear predicate was
  extracted and why fixing it fail-**closed** instead would be wrong, the render
  damage channel / wake path / schema note `openspec/config.yaml`'s design rules
  require, the help-context note its third rule requires, and the invariants
  table.
- **`tasks.md`** — checklist mapping to chunks 1–4 (chunk 4's items checked,
  1–3's left open for their coders), a **§5 scenario → test mapping** table
  covering all ten delta-spec scenarios, and a §6 validation section ending with
  the single pre-PR `just ci` task.
- **`specs/activity-signals/spec.md`** — `## ADDED Requirements`: _An explicit
  OSC attention signal is live state, not an inbox event_, with the five
  scenarios the chunk spec named (raised hand marks needs-you with an empty
  inbox; answering lowers it; a deliberate push still records a row; the opt-in
  holds one current row per session; a session with no worktree records nothing).
- **`specs/notifications/spec.md`** — `## ADDED Requirements`: _Clearing the
  inbox clears exactly what the inbox displays_, with the five scenarios (main
  checkout shown **and** cleared; another repo's known worktree neither shown nor
  cleared; untagged row shown and cleared; the all-worktrees view clears
  everything; clearing lowers the live hands).

Requirements went to `activity-signals` / `notifications` because there is no
live `openspec/specs/attention-signals/` capability — that change folder was
never synced. `add-osc-attention-signaling` was neither archived nor edited.

### 2. `docs/help/bars.md`, `docs/help/panel.md`

Prose only, in the surrounding voice, one addition of each kind per page:

- **what `a` covers** — `bars.md`'s inbox-keys parenthetical now says `a` covers
  this repo's rows plus the host-global ones, counting a row tagged to the repo's
  own main checkout as this repo's ("which it always displayed but never used to
  clear" — the fix), with `A`/`g` widening to every worktree. `panel.md`'s
  clear-all paragraph gains the same main-checkout sentence.
- **what a raised hand is** — a new short paragraph on each page: an agent's
  `OSC 9` / `OSC 777` "I need you" is live state shown by the sidebar dot and the
  `✋` chip, cleared when you answer, and absent from the notification list unless
  `[notifications] agent_attention_inbox` is on.

No frontmatter change on either page: no new action id, chord, zone or panel
section, so no `ACTION_SPECS` edit and no help-ratchet churn — as the chunk spec
predicted. Neither generated page (keybindings, config reference) was touched.

### 3. `CHANGELOG.md`

Two **Fixed** entries at the top of `[Unreleased]`, in the file's existing
`### Fixed — <headline>` + bullets style:

- _a raised hand is live state, not an inbox entry_ — the per-turn row is gone,
  answering now clears the demand, the one-time migration retires the existing
  backlog, deliberate pushes are untouched, and
  `[notifications] agent_attention_inbox` (env
  `THEGN_NOTIFICATIONS_AGENT_ATTENTION_INBOX`) is the opt-in with one current row
  per session.
- _"clear all" clears everything the inbox shows_ — including rows tagged to the
  repo's main checkout, with the fail-open/fail-closed asymmetry explained and
  the one-shared-predicate fix stated; another repo's known worktree is still
  neither shown nor cleared.

## Verification

| Gate                                                                | Result                                                                   |
| ------------------------------------------------------------------- | ------------------------------------------------------------------------ |
| `openspec validate fix-attention-signal-noise --strict`             | ✅ valid                                                                 |
| `openspec validate --all --strict` (the `just ci` gate)             | ✅ **166 passed, 0 failed**                                              |
| `test/brand-guard.sh`                                               | ✅ clean (no CHANGELOG exception needed — no old-brand token introduced) |
| `test/stale-docs-guard.sh`                                          | ✅ clean                                                                 |
| markdown formatting (`test/fmt/prettier-stable.sh`, treefmt's shim) | ✅ fixed point on all seven touched/created markdown files               |
| `test/help-ratchet.txt`, `-prose-`, `-context-`                     | ✅ unchanged (all three untouched in `git status`)                       |

`just openspec-validate` / `just lint` need the dev shell; `openspec` was run
hermetically as `nix run .#openspec` (same pinned build the justfile
passthrough uses).

**On the help-ratchet run.** The chunk's done criteria ask for
`cargo nextest run -p thegn-host -- help::ratchet`. That test's three arms are
keyed on things this chunk did not touch: `action_docs_ratchet` compares
`ACTION_SPECS`/core `BUILTINS` ids against pages' `actions:` frontmatter (no Rust
and no frontmatter changed); `every_panel_context_has_a_documentation_page` reads
`contexts:` frontmatter (unchanged); and `claimed_actions_are_mentioned_in_the_page_body`
can only be _helped_ by added prose, and its allowlist is currently empty, so its
"now written but still allowlisted" arm cannot fire. The run itself compiles
`thegn-host` on top of chunk 2's in-flight `thegn-core` edits in this shared
worktree — see the note below for what it reported.

## Notes for the coders on chunks 1–3

- The delta specs are the contract your tests are graded against; `tasks.md` §5
  names the test for each scenario. Where a chunk spec fixed a test name
  (`the_repo_main_checkout_has_no_registry_row_so_it_shows`,
  `scoped_clear_marks_rows_the_registry_does_not_know`, …) that name is used
  verbatim. The rest are prescriptions — if you name a test differently, update
  the mapping row rather than leaving it stale.
- `tasks.md` items 1.x/2.x/3.x are unchecked. Tick yours as you land them.
- `CHANGELOG.md` was edited by this chunk only; chunks 1–3 should not need to
  touch it. The two entries already describe the finished behaviour of all
  three, so if a chunk's final behaviour diverges, fix the entry rather than
  adding a third.
