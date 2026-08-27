# Chunk 4 — Specs, help prose, changelog

**Issue:** THE-68. **Branch:** `tg/the-68-log-noise`.
**Depends on:** nothing to compile; describes chunks 1–3.
**Land order:** fourth (write it early, finalise once 1–3 are settled).
**Overlaps:** no source files with any other chunk.

Read `.thegn/pipeline/architect/design.md` in full — this chunk turns §1–§3 into
the repo's own artifacts.

---

## Why this is its own chunk

The repo manages its development with OpenSpec, and `just openspec-validate`
(`openspec validate --all --strict`) runs in `just ci`. THE-68 changes behaviour
this repo already has a _written contract_ for — and the contract currently
agrees with us and the code does not, which is worth recording explicitly.

`openspec/changes/add-osc-attention-signaling/specs/attention-signals/spec.md`
says:

> **Scenario: Resume clears the signal** — WHEN the signaling process resumes
> producing output or the human focuses the pane THEN the needs-attention state
> clears.

The live half of the implementation satisfies that. The persisted half (the
`agent_attention` notification row) never did: it stayed unread and kept the
worktree `Blocked` until the user cleared it by hand. Chunk 3 makes the whole
implementation match the spec. Note also that this change folder is **still
in-flight and unarchived** while much of it has landed — do not archive it, and
do not edit its delta specs; write a new change.

There is no `openspec/specs/attention-signals/` live capability; the OSC
behaviour was never synced. The nearest live capabilities are
`openspec/specs/activity-signals/` and `openspec/specs/notifications/`.

---

## Files

### 1. `openspec/changes/fix-attention-signal-noise/` — NEW change folder

Structure per `CLAUDE.md` § _Spec-driven development_: `proposal.md`,
`design.md`, `tasks.md`, and delta specs under `specs/<capability>/spec.md` using
`## ADDED / MODIFIED / REMOVED Requirements` with `### Requirement:` (SHALL/MUST)

- `#### Scenario:` (WHEN/THEN). Check `openspec/config.yaml` for the schema, and
  copy the shape of a recent change folder rather than inventing one.

**`proposal.md`** — the two symptoms, the two root causes, the thesis ("an inbox
row is an event you might miss; a raised hand is live state you can already
see"). In **Impact**, cite the `tasks.md` roadmap group letter + number for the
notification/attention work (`CLAUDE.md` requires the link), and name the four
implementation chunks.

**`design.md`** — condense architect design §3–§5: the `session_attention` table,
its full lifecycle table, why the demand reuses `AgentNeedsInput` rather than
getting a new tier (the `stage_blocked_since` precedent), why the clear predicate
was extracted, and the invariants table.

**`tasks.md`** — checklist mapping to chunks 1–4, with the final "run `just ci`"
validation task (a pre-PR gate run once, per `CLAUDE.md` — not per edit).

**`specs/activity-signals/spec.md`** — `## ADDED Requirements`:

> ### Requirement: An explicit OSC attention signal is live state, not an inbox event
>
> A raised hand from `OSC 9` / `OSC 777;notify` SHALL be recorded as live
> per-session state and MUST NOT append a notification to the inbox by default.
> It MUST be lowered when the user's input reaches the process, when the session
> ends, and when the worktree's needs-you signal is acknowledged or cleared. It
> MUST raise the same blocked demand an explicit `agent_attention` notification
> raises, through the same attention reason, so no surface distinguishes them.
> Recording an additional inbox row SHALL be opt-in
> (`[notifications] agent_attention_inbox`), and when enabled MUST hold at most
> one current row per session rather than one per signal.

Scenarios (WHEN/THEN), each mapping to a test chunk 3 writes:

- a raised hand marks the worktree needs-you and leaves the inbox empty;
- answering the agent lowers the hand with no inbox interaction;
- a deliberate `agent_attention` push still records an inbox row;
- with the opt-in on, repeated signals from one session leave one row;
- a signal from a session with no worktree records no row.

**`specs/notifications/spec.md`** — `## ADDED Requirements`:

> ### Requirement: Clearing the inbox clears exactly what the inbox displays
>
> The repo-scoped inbox's "clear all" SHALL mark read exactly the set the
> repo-scoped inbox displays, evaluated by one shared predicate — untagged
> (host-global) rows, rows tagged with one of the repo's registered worktrees,
> and rows tagged with a worktree path the registry does not know (the repo's
> main checkout, an externally-created worktree). It MUST NOT mark read a row
> tagged with a known worktree of a different repo. Clearing MUST also lower the
> live raised hands for the same scope.

Scenarios:

- a row tagged to the repo's main checkout is displayed **and** cleared;
- a row tagged to another repo's known worktree is neither displayed nor cleared;
- an untagged row is displayed and cleared;
- the all-worktrees (`g`) view clears everything;
- clearing lowers the live hands for the same scope.

### 2. `docs/help/panel.md` and `docs/help/bars.md`

`bars.md:160` documents the inbox keys (`x` read, `d` dismiss, `a` clear all).
Add one sentence each, in the surrounding voice:

- **what `a` covers**: this repo's rows plus host-global ones (`A`/`g` widens to
  every worktree) — and that a row tagged to the main checkout counts as this
  repo's, which is the fix;
- **what a raised hand is**: an agent's `OSC 9`/`OSC 777` "I need you" shows as
  the sidebar dot and the `✋` chip and clears when you answer; it is not an
  inbox entry unless `[notifications] agent_attention_inbox` is on.

Keep it short. No new action id, chord, zone or panel section is introduced, so
**no `ACTION_SPECS` change and no help-ratchet churn is expected** — if
`crates/thegn-host/src/help/ratchet_tests.rs` complains, something in chunks 1–3
added an action it should not have. Never hand-write the keybindings or
config-reference pages; both are generated at runtime.

### 3. `CHANGELOG.md`

Two entries under the unreleased section, in the file's existing style:

- **Fixed** — the inbox no longer fills with one "agent is waiting for your
  input" row per agent turn; a raised hand is live state and clears when you
  answer. Mention the one-time migration that retires the existing backlog and
  the `[notifications] agent_attention_inbox` opt-in.
- **Fixed** — "clear all" now clears every notification the inbox shows,
  including rows tagged to the repo's main checkout, which were displayed but
  never cleared.

Check the repo's brand guard if `CHANGELOG.md` has an exception list — a prior
change tripped it here.

---

## Approach notes

- The OpenSpec CLI is hermetic and on PATH in `nix develop`; `just openspec <args>`
  is a passthrough. Run `just openspec-validate` until clean.
- Delta specs describe behaviour **after** chunks 1–3, so finalise wording once
  those are settled — but draft first, since a scenario that cannot be written
  is a design smell worth catching early.
- Do not archive `add-osc-attention-signaling` and do not edit its files.
- Do not touch `openspec/specs/` directly; deltas merge on `/opsx:sync`.

## Done criteria

- [ ] `just openspec-validate` (`openspec validate --all --strict`) passes.
- [ ] Every scenario in the two delta specs maps to a named test in chunk 1 or
      chunk 3 — list the mapping in the change's `tasks.md`.
- [ ] `cargo nextest run -p thegn-host -- help::ratchet` passes and none of
      `test/help-ratchet.txt`, `test/help-prose-ratchet.txt`,
      `test/help-context-ratchet.txt` gained a line.
- [ ] `just lint` clean (treefmt covers markdown; yamllint covers any YAML).
- [ ] The change folder's `tasks.md` ends with the single pre-PR `just ci` task.
