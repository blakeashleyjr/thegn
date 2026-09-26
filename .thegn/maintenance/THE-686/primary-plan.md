# Primary review + greenlight — THE-686

Reviewing row 602's investigation. **APPROVED. Implement it as written, with the
three decisions below settled.**

This is the strongest investigation artifact of the batch. You reached the
two-caller design independently, and you found the thing the primary's own failed
attempt missed and that the brief only gestured at:

> calling the revised admission predicate twice without comparing snapshots would
> silently admit a newly appearing ignored file

That is exactly right, and it is the whole defect in the naive fix. Your
`Refusal::Changed` + stored-observation design is the correct shape.

Two additions of yours the primary specifically endorses:

- **Parse the porcelain records rather than trusting the flag.** `!!` is
  ignored-only; **anything unrecognised is `Dirty`**, not ignorable. Fail-closed
  on malformed records is the right default for a predicate that authorises
  deletion.
- **Preserving the existing 2 MiB bound as a conservative refusal.** Do not raise
  it, and do not add a code path that succeeds when the bound is hit.

## Decision 1 — raw bounded bytes, not a digest

Use the raw bounded status bytes, as you recommend. No new hashing dependency for
this.

On your ordering question: `git status --porcelain` is deterministically ordered,
and `--ignored=matching` reports the ignored _directory_ (`target/`) rather than
each file beneath it — so an in-progress build churning inside `target/` does not
change the observation, while a newly appearing top-level ignored entry does.
That is the behaviour we want, so a byte comparison is sound here. Do not sort or
normalise the bytes: if git's output ever does reorder, a spurious **keep** is the
safe direction, and normalising would mask a real change.

## Decision 2 — the two refusals get two different messages

Do not reuse one string. They are different facts and an operator needs to tell
them apart:

- **admission-time tracked/untracked** ⇒ `kept <branch> — edited since landing`
- **revalidation-time changed snapshot** ⇒ `kept <branch> — changed during cleanup`

Your plan routes the changed case to "edited since landing". That would be untrue:
a changed snapshot means something wrote in the directory _while the sweep was
running_, which is a concurrency signal, not a statement about user edits. Both
labels satisfy the issue's criterion; these two are also accurate.

Keep `swept <branch> (discarded build state)` as you proposed for the
ignored-only removal, and plain `swept <branch>` when nothing was discarded.

## Decision 3 — `force` is unchanged, including for ignored-only

`--force` bypasses the TTL clock and nothing else. So `force` + ignored-only ⇒
swept; `force` + tracked ⇒ kept; `force` + untracked-non-ignored ⇒ kept; `force`

- changed-during-cleanup ⇒ kept. Do not add a flag that discards real work.

## Scope confirmations

Your affected-files list is approved, including the `cmd/merge.rs` output change
and the conditional `handlers/merge_queue.rs` touch **only if** the in-app summary
genuinely needs the new category — check, and if it consumes counts only, leave
it and say so.

Out of scope, as you correctly identified: THE-685's filter logic (preserve it,
and the canary test that proves an unused driver never runs), TTL arithmetic,
branch-deletion/hold behaviour, submodule and special-index guards, and the
missing-landed-identity defect — that one is now **THE-687**, filed by the primary
with the DB evidence. If you learn something about it, note it; do not fix it.

## Tests

Your five-group matrix is the requirement, not a suggestion. Two emphases:

- Group 4 is the one that actually proves the issue: the full landed-worktree
  matrix across clean / ignored-only / tracked-modified / untracked-non-ignored,
  **each with `force = false` (expired row) and `force = true`**. Eight cases. Do
  not report the row finished with the force half missing.
- Group 3 must keep the replacement-directory identity failure. That assertion is
  about a different guard and must not be weakened while you change its
  neighbour.

Keep the fixture rules: nextest for the process-wide-env tests,
`-c commit.gpgsign=false` on fixture git commands, and never commit on the feature
branch inside a fixture before `Fixture::probe()` checks ancestry.

## Validation

Attempt `nix develop --command cargo check -p thegn-host --all-targets` and a
narrow `cargo nextest run -p thegn-host merge_cleanup`. **The pipeline sandbox
mounts `/nix/store` read-only, so this usually fails outright** — if it does, say
exactly that and stop. The primary runs all Rust validation, including clippy,
centrally. Never report `implementation-ready` for code you could not compile;
state what you could not run.

One warning from this batch, at the primary's expense: clippy under `-D warnings`
caught 13 findings across five lanes that neither the workers nor the reviewers
saw, including a stale `#[expect]` and a `dead_code` that flagged a real scope
question. Write code you expect to survive `clippy --all-targets -- -D warnings`:
no nested match that collapses, no `unwrap` after an `is_none` check, no
five-element tuple without a named alias, and tests last in the file.
