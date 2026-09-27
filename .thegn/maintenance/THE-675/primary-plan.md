# Primary decision — THE-675: NOT PROCEEDING (reviewing row 639)

**Accepted, and this row is complete.** The brief said a recommendation not to
proceed, with the counts behind it, was an acceptable and useful plan. You
produced exactly that, and the primary agrees.

## The measurement that settles it

- **53 direct production `util::git_out` call sites** in the scoped inventory.
- **0 of them are `cat-file` / `--batch`-eligible object reads.**

A persistent `git cat-file --batch` would therefore serve **zero** call sites. The
1.9 ms floor the issue measured came from `git rev-parse HEAD`, and `rev-parse`,
`status`, `rev-list` and `for-each-ref` are exactly the calls a batch process
cannot answer. The issue's payoff argument does not survive its own measurement.

That is a 1–2 day task retired with evidence, which is worth more than the task
would have been.

## Why this is not deferred but closed

Nothing about this changes if we revisit it later: the call-site mix is the
reason, not the implementation difficulty. The work would only become worthwhile
if thegn started reading git **objects** in volume, and if that ever happens the
measurement is the thing to redo first. The `--batch-check` variant I asked you to
price is likewise moot at zero eligible sites.

**No implementation is authorized.** Do not open a follow-up implementation row.

## Noted from your report

- **THE-669's parent meaning is unresolved.** Correct to flag rather than guess; it
  does not affect this outcome. The primary leaves it unresolved — a parent epic
  whose scope is unclear is not a reason to do unmeasured work.
- **THE-171's lifecycle changes stay untouched.** Correct, and now moot.

## What lands from this row

The investigation artifact only. It is the record of the measurement, and it is
the deliverable — do not add production code, tests, benchmarks or config.
