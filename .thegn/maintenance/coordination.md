# Coordination brief — THE-706

## How this lane is run

You are one lane of a maintenance batch. A primary agent (the Lead) reviews
every plan and every diff before anything merges. Your job is this issue and
nothing else.

### Cargo — you MAY run these, and should

The stage prompt forbids Cargo. The primary relaxes that for exactly two
commands, because a report of `implementation-ready` for code that does not
compile is worthless:

    nix develop --command cargo check -p <crate> --all-targets
    nix develop --command cargo test  -p <crate> --lib <narrow filter>

Run them as often as you need. Everything else — `build`, unfiltered `test`,
`nextest --workspace`, clippy, any `just` recipe — stays the primary's.

**Attempt it; if `nix develop` fails inside the sandbox, report that verbatim
and stop.** The pipeline sandbox read-only-binds `/nix/store`, so `nix develop`
sometimes cannot create its temporary store paths. That is an environment
limitation, not your failure, and "checks not run, nix develop failed with
<error>" is a perfectly good report. What is NOT acceptable is claiming the
code compiles without having checked.

### Database safety

**Never run a thegn command that opens or migrates the real database.**
`$XDG_STATE_HOME` defaults to the user's live state and a migration from your
shell mutates the running instance's DB. Any test needing a DB must build its
own fixture in a temp dir.

### `.thegn/maintenance/` hygiene

This directory carries previous batches' records. You may add files under
`.thegn/maintenance/<ISSUE>/`. **Do not delete anything** — a deletion on this
branch deletes it from main when the lane merges.

### Ratchets that a change like this trips

`just lint` / `just test` enforce shrink-only allowlists. Expect to need an
update in the same change for: a new `section.key` in config (three ratchets —
`config.toml.example` key coverage, the env-overlay list, and the completion
slot), a new capability row (surface-gaps), a new action id (help page
`actions:` frontmatter _and_ prose), a control-API schema snapshot
(`THEGN_UPDATE_SNAPSHOTS=1 cargo test -p thegn-svc --test control_schema`),
platform `#[cfg]` outside `platform/`, colour/glyph literals outside the caps
chokepoints, and ignored `Result`s. Never add a ratchet entry without a written
reason.

### Report vocabulary

Exactly one of: `plan-ready`, `implementation-ready`, `source-review-clear`,
`revisions-needed`. Never PASS/APPROVED.

## Scope for THE-706

**In scope:** make `worktrees.branch` stop being an input to any decision, by
reconciling against git on the read path.

**Constraints the primary is setting:**

1. **Reconcile on read; do not add a watcher.** The issue's own suggested
   direction is right: one `git worktree list --porcelain` gives every
   worktree's real HEAD at once. Find the existing hydration path that already
   walks worktrees and put it there. A new background thread or fs-watch for
   this would be the wrong shape — and this repo's 0%-idle contract means a new
   wake source needs a much stronger justification than a display label.
2. **Nothing blocking on the event loop.** `git` is subprocess I/O; it must not
   run on the render/input thread. If the natural place turns out to be on-loop,
   say so and stop rather than adding a blocking call — that is a design
   question for the primary.
3. **Audit the readers before changing them.** Grep every use of the `branch`
   column and classify each as _display_ or _decision_. The issue names the
   candidates (sidebar label, merged-worktree sweep eligibility, `wt list`,
   dispatch/pipeline logic). Report the classification in your plan; a fix that
   reconciles one caller and leaves three is not the fix.
4. **A migration is explicitly NOT wanted.** The 5 divergent rows are correct
   history; the read path is what is wrong. Do not add a schema version.
5. The **held-branch part of the acceptance criteria is where the user-visible
   value is.** `BranchRow` has no notion of "held by another worktree", so the
   UI offers deletes git will refuse. Adding that field and making the menu
   either hide or explain the refusal is in scope and should be tested.

**Out of scope:** THE-697's delete-race work itself, and any change to how
worktrees are created or named.

**Testing note:** the repo has a known bug where shelling out to `thegn wt new`
in a test mutates the _real_ repo. Build git fixtures with plain `git` commands
in a temp dir, and pass `-c commit.gpgsign=false` to every fixture commit —
inheriting the user's global config otherwise hangs the test on a GPG prompt.
