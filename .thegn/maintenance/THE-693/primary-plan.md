# Primary review + greenlight — THE-693

Reviewing row 617's investigation. **APPROVED. Direction 1 as you recommend, with
the scope boundary settled below.**

This is the best investigation of the chain. You did the three things the brief
asked and the previous lanes mostly did not:

- **Quoted THE-440 verbatim** rather than paraphrasing, and then reasoned from what
  it actually forbids. Your reading is right, and it settles the direction.
- **Found that there is no single authority** — `SkillRegistry` + the harness seam,
  host `MQ_COMMANDS`, and the OpenSpec dev-shell writer are three separate owners.
  That absence _is_ the defect; everything else is a symptom.
- **Reached the aggregation rule independently**: "must not return clean after
  seeing the first seeded path". That is the sentence that keeps this from becoming
  a data-loss bug.

Your safety stance is also exactly right and is hereby the contract: _an unknown
file under a known directory, a modified managed file, or a path from a writer
outside the authority must remain protected user work._ Do not weaken it.

## The drift you inferred is already real — measured

You reasoned the two lists could drift. They already have, and knowing the concrete
shape will help you build the authority:

```
thegn/.git/info/exclude:26  .claude/skills/mq/          present
cms/.git/info/exclude       .claude/skills/mq/          MISSING
                            (.agents/skills/mq/ and .pi/skills/mq/ ARE present)
```

The single untracked file across all 11 `cms` worktrees is
`?? .claude/skills/mq/SKILL.md`. So one omitted line, in one of three parallel
families, disables cleanup for a whole repository. This also confirms your point
that `exclude_locally` "explains why this repository often hides the problem" —
thegn additionally has `.claude/*` in its committed `.gitignore`, which is the real
reason its own worktrees sweep.

Use this as a test case: the fixture must reproduce a repo where
`.claude/skills/mq/SKILL.md` exists and is **not** ignored.

## DECISION — scope the authority to the Rust-owned seeders

The OpenSpec dev-shell writer is outside the shipped Rust code, so it cannot be
part of a Rust authority without new plumbing. Settle it this way:

- **In scope:** one shared authority covering the seeders that live in Rust —
  `SkillRegistry`/harness skill roots and the `MQ_COMMANDS` table. `merge_cleanup`
  consumes that authority and must not duplicate `.claude`, `.agents` or `.pi`
  literals, exactly as you say.
- **Out of scope, deliberately:** making the nix dev-shell/OpenSpec writer register
  its paths. Files it generates fall under your "unknown generated path" rule and
  therefore **keep the worktree** — conservative, correct, and honest.
- This is sufficient for the reported case: the primary checked, and the `cms`
  worktrees contain **no** opsx-generated files. Covering the Rust seeders unblocks
  them. If you find a repo where dev-shell output does block cleanup, **report it
  as a follow-up finding** rather than widening this lane.

Say in your report which paths the authority ends up owning, so the boundary is
written down rather than implied.

## Content validation — approved, and here is why it matters concretely

Your call to carry expected managed content or a hash is right, and it is not
theoretical. Skill files under `.claude/skills/` are edited by hand in practice —
this session's own work involved editing a SKILL.md. If a formerly-seeded file that
someone has since edited were classified as tool state, the sweep would delete real
work. So:

- Managed path **and** content matches ⇒ tool state.
- Managed path, content differs ⇒ **user work**, keep the worktree.
- Path not in the authority ⇒ user work, keep the worktree.

Keep the comparison bounded, and make a file you cannot read count as user work,
not as a match.

## The message must stop lying

`Refusal::Dirty` renders as "edited since landing". A worktree held by something
thegn wrote must not say that. Either it sweeps, or the refusal names what is
actually holding it — and if it is an unrecognised generated file, say so, because
that is the message that would have made this whole issue a five-minute diagnosis
instead of a chain.

## Confirmed as written

Keeping the existing `!!` ignored-only handling from THE-686; classifying every
status record before accepting the tree; leaving the existing exact `exclude_locally`
behaviour in place as legacy compatibility without expanding it into a blanket
`.claude/*` workaround; and not touching any repository's committed `.gitignore`.

## Out of scope

THE-689, THE-691. THE-692 (`thegn land` records no landed row) — it landed while you
were investigating; merge current `main` and do not touch `cmd/land.rs`. The six
earlier landed fixes in this chain: preserve them.

## Tests

Your matrix, plus the `--force` half for each, which is a matrix and not four
tests. Do not report the row finished without it. Specifically:

- seeded-only in a repo that ignores the paths (today's passing case) — still sweeps;
- seeded-only in a repo that does **not** ignore them — now sweeps;
- seeded path whose **content was modified** — kept;
- unknown file under a seeded directory — kept;
- seeded content **plus** a real untracked user file — kept;
- seeded content **plus** a tracked modification — kept.

Traps: nextest only (`TestIsolation` mutates process-wide env);
`-c commit.gpgsign=false` on fixture git commands; `merge_sweep::due` treats
`merged_ttl_secs = 0` as _never sweep_, so clock-only cases need `ttl = 1` plus a
real wait; and `just smoke` has caught three things in this chain that clippy and
9200+ unit tests did not.

Above all: **a fixture built from the same mental model as the code proves nothing.**
THE-690's parser passed its unit tests and its own smoke shim and still refused
everything against the real `docker`. Your fixture must reproduce the
non-ignoring repo, which thegn's own `.gitignore` hides.

## Cargo

Attempt `nix develop --command cargo check -p thegn-host --all-targets` and a narrow
`cargo nextest run -p thegn-host merge_cleanup`. **The pipeline sandbox mounts
`/nix/store` read-only, so this usually fails outright** — say exactly that and stop
if so. The primary runs clippy, the full workspace nextest and smoke centrally.

Never report `implementation-ready` for code you could not compile; say what you
could not run. Every lane in this chain shipped something that did not compile, the
most recent being a test that reached for a private `Db::conn`. The primary caught
each. That division of labour is expected — report honestly.
