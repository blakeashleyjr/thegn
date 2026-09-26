# Coordination brief — THE-687

Third cause in the merged-worktree sweep chain. THE-685 (`0a3e3150`) and THE-686
(`2acdf364`) have both **landed and are verified gone from the live sweep**. Four
worktrees now fail on this issue and nine on THE-688; the sweep still collects
nothing, which is a bug the repo owner hits every day.

## What the primary already verified — do not redo it, build on it

The evidence in the issue body was measured by the primary today against the live
database, not copied from a stale audit. Specifically:

```
SELECT COUNT(*), SUM(result_oid IS NULL OR result_oid='')
FROM merge_queue WHERE status='landed';        -- 33 | 4
```

All four bad rows have `updated_at` **within the same two seconds**
(2026-09-11 21:47:24–25) while contemporaneous rows carry a real OID. That rules
out "the column was added later" and points at **one bulk writer**.

And this state is supposed to be impossible: `db_aux.rs:119-124` asserts
`Landed => result_oid.is_some_and(|oid| !oid.is_empty())`, and
`validate_merge_outcome` is called at `db_aux.rs:348`, immediately before the
`merge_queue` upsert at `:364`. **So the guarded writer did not produce these
rows.**

## Task 1 — find the writer. This is the part that needs real investigation.

Do not assume; the same-second timestamps are the lead. Candidates, all
unverified:

- the fold-actor / `thegn land` path, already known to land via plumbing that
  skips gates;
- a reconcile/sync path that bulk-marks branches landed from git ancestry, where
  no merge commit was recorded because the landing never went through the queue;
- a writer that existed on 2026-09-11 and has since been removed — in which case
  say so with the commit, and the fix is only the consumer half plus a
  migration/backfill.

Whatever you find, the outcome is a test that fails if **any** path can write
`status='landed'` with an empty or NULL `result_oid`.

## Task 2 — the consumer is also wrong, and this half is not optional

`merge_sweep.rs:203` turns a missing cache field into a permanent refusal:

```rust
let Some(landed) = row.result_oid.as_deref() else {
    report.kept.push((entry.branch.clone(), "landed commit identity is missing".into()));
    continue;
};
```

CLAUDE.md is explicit that **git is the source of truth for worktrees and the
forge for PRs; SQLite is a cache + resurrection layer.** The landed commit is
derivable from git for any branch that genuinely merged. Derive it when the cache
lacks it, and write the derived value back so the derivation happens once.

Fixing only the writer leaves these four stuck forever, because the bad rows
already exist. Fixing only the consumer lets the invariant violation recur. **Both.**

## The hard safety line

A missing `result_oid` must **never** weaken the merged-ness proof. The landed OID
feeds an ancestry check; if you cannot prove the branch is merged into the target
from git, the refusal stays. "Derive it" means _derive the real merge commit_, not
"skip the check when the cache is empty". A sweep that deletes an unmerged
worktree destroys work that exists nowhere else.

`--force` bypasses the TTL clock and nothing else. Do not touch that.

## Out of scope

- **THE-688** — `runtime/session/dispatch ownership requires explicit cleanup`,
  caused by a persisted `tab_groups` layout row. Separate lane, running in
  parallel with yours; you will both touch the sweep area, so keep your diff
  narrow and do not "fix" that predicate.
- THE-685's filter-driver logic and THE-686's status-observation/`Refusal::Changed`
  design — both just landed. Preserve them; do not refactor them.
- TTL/grace arithmetic, branch-deletion holds, submodule and special-index guards.

## Acceptance criteria (from the issue)

- [ ] The writer producing `landed` + NULL/empty `result_oid` is identified and
      fixed, with a test that fails if any path can write that pair.
- [ ] The sweep derives the landed commit from git when the cache lacks it, and
      refuses only when merged-ness genuinely cannot be proven.
- [ ] A derived identity is written back to the cache.
- [ ] The four existing rows become sweepable **without manual DB surgery.**
- [ ] A branch not merged into the target is still never swept, with or without
      `--force`.
- [ ] Tests cover: cached OID present; absent but derivable; absent and not
      derivable (still refused).

## Testing traps, measured in this area today

- **Use nextest, never `cargo test`.** `TestIsolation` mutates process-wide env,
  so threaded `cargo test` cross-contaminates — a `gate_runner` test failed under
  `cargo test` and passed under nextest, and the primary nearly chased it as a
  regression.
- Fixture git commands need `-c commit.gpgsign=false`; global signing is on and
  an unconfigured fixture hangs ~120s instead of failing.
- `Fixture::probe()` verifies the branch is merged into main. Committing on the
  feature branch inside a fixture makes it _unmerged_, so the refusal you get is
  not the one you were testing.
- **`just smoke` is a real gate here, not a formality.** THE-686's change was
  clippy-clean and passed 9106 unit tests, and `test/smoke.sh` still caught a case
  that encoded the old contract by name. If you change sweep behaviour or output,
  grep `test/smoke.sh` for the strings you are changing.

## Cargo

Attempt `nix develop --command cargo check -p thegn-host --all-targets` and a
narrow `cargo nextest run -p thegn-host merge_sweep`. **The pipeline sandbox
mounts `/nix/store` read-only and this usually fails outright** — if it does, say
exactly that and stop. The primary runs all Rust validation, including clippy and
smoke, centrally. Never report `implementation-ready` for code you could not
compile; say what you could not run. Last lane's implementation did not compile
(an ambiguous `Vec::new()` in a test) and the primary caught it — that is the
expected division of labour, not a failing, so report honestly.
