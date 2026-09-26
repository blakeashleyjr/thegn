# Primary review + greenlight — THE-687

Reviewing row 605's investigation. **APPROVED, with one substantial correction to
the derivation — read that section before you write any code.**

Your writer hunt answered the question the issue could not. You found three
queue-status writers that can record `landed` with no effective OID —
`merge_driver.rs:306-317`, `cmd/merge.rs:834-838`,
`handlers/merge_queue.rs:1102-1108` — while `db_aux.rs`'s `persist_merge_outcome`
validation never permits that pair. That explains the same-second bulk
transition, and your step 2 (a central guard on both `replace_merge_status` and
`update_merge_status`, allowing a NULL argument only where an existing nonempty
OID is preserved by the legacy COALESCE API) is the right shape. Steps 1–4 are
approved as written, subject to the correction below.

## CORRECTION — "the merge commit whose second parent is the branch tip" does not work

You asked the primary to confirm the four Git graph shapes and the exact
first-parent/fast-forward derivation. The primary measured all four against
current `main`, and the answer invalidates the obvious derivation — including the
one the **issue body itself suggests**. Take this as the authority over the issue
text.

All four tips are genuine ancestors of `main`, so all four are legitimately
landed:

| branch           | tip        | shape                                                                                                                                                               |
| ---------------- | ---------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `tg/spark-radar` | `302e2c1c` | direct parent of an **octopus** merge `47aa962e` (**five** parents) — and it sits in slot **2** only by luck                                                        |
| `tg/keen-marble` | `bfb1a471` | **not a parent of any merge.** First `main` commit containing it is `27a995ff`, an ordinary non-merge commit (`fix(panes): …`), itself parent **5** of that octopus |
| `tg/bold-petal`  | `cb27b3c5` | **not a parent of any merge.** First containing commit is `a3ff446c`, `Merge branch 'main' into audit/bold-petal-review`                                            |
| `tg/bold-mango`  | `a54afff4` | **not a parent of any merge.** First containing commit is `24bafac7`, `Merge branch 'main' into audit/bold-mango-review`                                            |

So **three of the four rows this issue exists to fix have no merge commit with the
tip as a parent at all** — their work was absorbed into a review branch, or into
another branch's history, which was merged later. A `^2` derivation fixes exactly
one row and silently leaves three refused forever. A derivation that scans all
parents of every merge (necessary anyway, because of the octopus) still fixes only
one.

### The derivation to implement

**The landed identity is the earliest commit on the target that has the branch tip
as an ancestor**, i.e. the commit that first carried it onto the target:

```
git rev-list --ancestry-path --reverse <tip>..<target> | head -1
```

That is well defined for every shape above — for a plain merge it _is_ the merge
commit, for a fast-forward it is the tip's own position on the chain, and for the
three absorbed cases it is the commit that actually brought the work in. Use one
shared helper, as you proposed, so the sweep and the writers agree.

Do not special-case first-parent vs octopus vs fast-forward as separate
strategies; that is how the `^2` assumption crept in. One ancestry-based
definition covers all of them. If you find a shape it does not cover, report it
rather than adding a second heuristic.

Bound and document the cost: `--ancestry-path` walks history, which is acceptable
once per swept worktree but must not end up inside a loop over all rows.

### Why this stays safe

The merged-ness proof is `git merge-base --is-ancestor <tip> <target>`, and it is
independently TRUE for all four. **The ancestry guarantee must never depend on the
derivation succeeding.** Check ancestry first; derive second; if ancestry fails,
refuse regardless of what any cache says. A squash-merge landing leaves the tip
_not_ an ancestor, so it must stay refused — that is correct fail-closed
behaviour, not a gap, and it is a real case even though none of these four hit it.

Say so in the report if a derivation succeeds for a row whose ancestry check
fails; that combination would mean the helper is wrong.

## Restated from the brief

- Both halves, not one. Fixing only the writers leaves these four stuck forever
  because the bad rows already exist; fixing only the consumer lets the invariant
  violation recur.
- Your step 3's compare-and-write/backfill keyed on worktree, branch, target,
  landed status and observed timestamp is approved, including rejecting empty
  derived strings and reloading the row afterwards so cleanup's exact-identity
  recheck sees the new OID and timestamp.
- `--force` bypasses the TTL clock and nothing else.

## Scope

Out of scope: **THE-688**, running in parallel — it removes the
`has_persisted_worktree_session` veto in `merge_cleanup.rs:86-93` and adds
`tab_groups` teardown. Do not touch that predicate or the layout tables. Also
preserve THE-685's filter-driver logic and THE-686's `StatusObservation` /
`Refusal::Changed` design; both landed and were reviewed.

Keep the report vocabulary THE-686 established — `swept … (discarded build
state)`, `kept … — edited since landing`, `kept … — changed during cleanup`. A
derived-and-backfilled identity needs no new report line.

## Tests

Your matrix plus these four, which the graph evidence above makes mandatory:

- cached OID present — unchanged behaviour and ancestry proof;
- absent, landed via a **plain two-parent merge** — derives the merge commit;
- absent, landed via an **octopus merge** where the tip is **not** parent 2 —
  this is `tg/spark-radar`'s real shape and a `^2` implementation passes the
  previous case while failing this one;
- absent, tip **absorbed** into another branch that was merged, so no merge has
  it as a parent — this is the shape of **three of the four** rows;
- absent and **not merged** — still refused, with and without `--force`.

## Validation

Attempt `nix develop --command cargo check -p thegn-host --all-targets` and a
narrow `cargo nextest run -p thegn-host merge_sweep`. **The pipeline sandbox
mounts `/nix/store` read-only and this usually fails outright** — if it does, say
exactly that and stop. The primary runs all Rust validation, including clippy and
smoke, centrally.

Grep `test/smoke.sh` for the sweep strings you touch: on THE-686 the
implementation was clippy-clean and passed 9106 unit tests, and smoke still caught
a case that encoded the old contract by name.

Never report `implementation-ready` for code you could not compile; state what you
could not run. THE-686's implementation did not compile (an ambiguous `Vec::new()`
in a test) and the primary caught it — that is the expected division of labour, so
report honestly rather than optimistically.
