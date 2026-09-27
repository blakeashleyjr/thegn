# Coordination brief — THE-693

Seventh cause in the merged-worktree sweep chain, and the one that makes the whole
feature work only in this repo. Six causes are landed (THE-685 `0a3e3150`,
THE-686 `2acdf364`, THE-687 + THE-688 `e1a64f61`, THE-690 `052a4bd9`+`6b7f7cc9`;
THE-692 is in review) and `thegn merge sweep` collects **in thegn**. Run it in any
other workspace and it collects nothing.

## NAMING TRAP — read this first

Commit `50b32ab2` on main is labelled **`fix(the-693)`** and is **NOT this issue**.
It is an unrelated grace-clock fix; `the-693` was a placeholder branch name that
happened to collide with the number Linear later assigned here. Do not read that
commit as prior work on `.claude/` seeding, and do not assume any of this is
already done.

## What the primary already measured — build on it

In the `cms` workspace, all 11 merged worktrees refuse with **"edited since
landing"**. The operator edited nothing. Measured on three of them:

```
tracked edits:          0
untracked non-ignored:  1
    ?? .claude/
```

`.claude/` contains exactly `commands/` and `skills/` — **content thegn seeds
itself**, per CLAUDE.md (the `.claude/` commands and skills are seeded per
checkout, and the bundled `/pipeline` and `/supervise` skills go into every
worktree's `.claude/skills/`).

Whether that seeded content blocks cleanup forever depends entirely on the target
repo's ignore rules:

```
thegn:  git check-ignore -v .claude/commands  ->  .gitignore:21:.claude/*
cms:    git check-ignore -v .claude/commands  ->  NOT ignored
```

thegn ignores `.claude/*`, so THE-686 classifies its worktrees as ignored-only and
they sweep. `cms` has no mention of `claude` in `.gitignore`, so the seeded
directory is untracked-non-ignored, which is `Refusal::Dirty`.

**So thegn plants the blocker and then refuses to collect the worktree because of
it.** It can only clean its own worktrees because its own repo happens to ignore
the files it writes. Re-verify these citations and report them confirmed.

## The decision the primary needs from you — investigate both, recommend one

Do **not** just pick one. The two directions have a real conflict and the
investigation must surface it:

1. **Teach the cleanliness predicate about thegn's own seeded paths.** Classify a
   known set as tool state, the same category THE-686 established for ignored
   build output. Cost: the predicate that authorises **deletion** grows an
   allowlist. Anything on that list can no longer protect a worktree, so the list
   must be exact, and it must come from the same authority the seeder uses — two
   drifting lists would be a deletion bug.
2. **Make the seeded paths genuinely ignored** by writing them to
   `.git/info/exclude` at seed time, letting THE-686's existing ignored-only logic
   handle them with no new special case. Cost: `info/exclude` lives in the **common
   dir**, so this mutates shared repository state — and **THE-440 ("seed agent
   permissions without mutating repository") established the opposite principle and
   has landed.** You must check what THE-440 actually forbids before proposing
   this; if it rules this out, say so and recommend (1).

Report which you recommend **and why the other was rejected**, with the THE-440
constraint quoted rather than paraphrased.

## Find the real authority, do not guess the path list

`.claude/commands` and `.claude/skills` are what the primary observed. They are
almost certainly not the whole set — `.agents/` and `.pi/` were also present as
ignored entries in a thegn worktree earlier in this chain. **Find the seeding code
and enumerate what it actually writes**, rather than hard-coding the two paths
observed from outside. If the seeder has no single list, that absence is itself a
finding worth reporting: the fix needs one authority.

## The second defect — the message

`Refusal::Dirty` renders as **"edited since landing"**, a claim about something the
operator did. `thegn merge sweep --help` defines the protected thing as a worktree
"you have gone back to and edited". Tool-seeded scaffolding is not that, and an
operator following that message goes looking for changes that do not exist.

Either such a worktree sweeps, or it is refused with a reason that names what is
actually holding it. Do not leave a refusal that misattributes thegn's own writes
to the user.

## Hard safety line

This guard authorises deleting a directory.

- A worktree with **genuine** tracked modifications or **genuine** untracked user
  files must still never be swept, with or without `--force`.
- A worktree with seeded content **and** real user work must never be swept. Test
  the mixture explicitly — a per-path classifier that returns "clean" as soon as it
  finds one seeded path would be a data-loss bug.
- `--force` bypasses the TTL clock and nothing else.

## Out of scope

- THE-689 (accepted cleanup races), THE-691 (admission not uniform across the CLI),
  THE-692 (land records no landed row — in review; do not touch `cmd/land.rs`).
- The six landed fixes in this chain. Preserve them; do not refactor.
- Changing any repository's committed `.gitignore`. The fix belongs in thegn, not in
  asking every project to accommodate it.

## Acceptance criteria

- [ ] A merged worktree whose only non-ignored content is thegn-seeded is swept
      once its TTL has elapsed, **in a repo that does not gitignore those paths**.
- [ ] Genuine tracked modifications or genuine untracked user files are still never
      swept, with or without `--force`.
- [ ] Seeded content **plus** real user work is never swept.
- [ ] No refusal reports tool-seeded content as "edited since landing".
- [ ] The seeded-path set has one authority shared by seeder and consumer.
- [ ] If direction 2 is chosen, the THE-440 interaction is documented and the
      mutation justified or avoided.
- [ ] Tests cover: seeded-only in an ignoring repo; seeded-only in a
      non-ignoring repo; seeded + untracked user file; seeded + tracked
      modification — each with and without `--force`.

That is a matrix, not four tests. Do not report the row finished with the `--force`
half missing.

## Testing traps, measured in this area

- **Use nextest, never `cargo test`.** `TestIsolation` mutates process-wide env.
- Fixture git commands need `-c commit.gpgsign=false`; global signing is on and an
  unconfigured fixture hangs ~120s instead of failing.
- `Fixture::probe()` verifies the branch is merged into main; committing on the
  feature branch inside a fixture makes it _unmerged_.
- **`merge_sweep::due` treats `merged_ttl_secs = 0` as "never sweep"**, so a
  clock-only assertion needs `ttl = 1` plus a real wait. A helper named
  `sweep_fixture_due` used 0 and made several cases vacuous.
- **`just smoke` is a real gate here** — it has caught three things in this chain
  that clippy and 9200+ unit tests did not.
- **A fixture built from the same mental model as the code proves nothing.**
  THE-690's parser passed its unit tests and its own smoke shim and still refused
  everything against the installed `docker`. Your fixture must reproduce a repo
  that does **not** ignore `.claude/`, which is the condition thegn's own repo
  hides.

## Cargo

Attempt `nix develop --command cargo check -p thegn-host --all-targets` and a
narrow `cargo nextest run -p thegn-host merge_cleanup`. **The pipeline sandbox
mounts `/nix/store` read-only, so this usually fails outright** — say exactly that
and stop if so. The primary runs clippy, the full workspace nextest and smoke
centrally.

Never report `implementation-ready` for code you could not compile; state what you
could not run. Every lane in this chain shipped something that did not compile, and
the primary caught each. That division of labour is expected — report honestly.
