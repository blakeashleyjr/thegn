# Coordination brief — THE-686

Merged-worktree sweep still collects nothing. THE-685 fixed the _first_ refusal
in this chain (filter drivers, landed `0a3e3150`); this is the _second_:
ignored files are treated as dirtiness, so every worktree that was ever built
is permanently unsweepable and `merged_ttl_secs` / `on_landed = "expire"` are
dead configuration.

This is a **live bug the repo owner is hitting right now** — their sidebar keeps
every merged branch forever. Correctness matters more than speed here.

## Read this before you plan: the primary already tried the obvious fix and it failed

The primary's first attempt was to drop `--ignored=matching` from the status
probe in `crates/thegn-host/src/merge_cleanup.rs`. **It broke 6 of 18 tests in
`merge_cleanup_tests.rs` and was reverted.** Do not re-attempt it as-is; it is a
dead end for a reason that the design has to address head-on.

The reason: **`clean()` has two callers with two different questions**, and the
one-line change conflates them.

1. **Admission** — `clean()` inside the sweep decision: _is this worktree a
   candidate at all?_ THE-686 is about this one. Here ignored-only content must
   stop blocking.

2. **Final validation** — `merge_cleanup.rs:516`, `clean(&path)?` inside
   `Verified::remove()`, a TOCTOU re-check run immediately before the directory
   is deleted: _has anything changed since admission?_ Here emptiness is not the
   real question, and the existing test
   `final_validation_preserves_new_ignored_files_and_replaced_directory`
   (`merge_cleanup_tests.rs:328`) asserts that an ignored file **appearing after
   admission** aborts the removal.

That second test is not obsolete and must keep passing on its merits. An ignored
file appearing _between_ admission and deletion means something is **writing in
that directory right now** — a build is running. Aborting is correct. That is a
different proposition from "ignored files exist", which is merely "a build ran
here once".

**So the design the primary expects is: make final validation compare against
what admission observed, not against emptiness.** Carry the admission-time
observation (the status output, or a digest of it) in the `Verified` token and
re-compare at removal time. A _change_ still aborts; an unchanged ignored-only
worktree proceeds. If you see a better way to separate the two questions,
propose it — but you must explicitly say how both roles stay correct.

## The other test that encodes the old semantics

`dirty_ignored_untracked_and_unknown_status_never_mean_clean`
(`merge_cleanup_tests.rs:227`) loops over `["tracked", "untracked", "ignored"]`
and asserts **all three** produce `Err(Refusal::Dirty)`. Its `ignored` case is
exactly the behaviour THE-686 changes.

**Revise that case deliberately and rename the test to match its new meaning.**
Do not delete the test, do not weaken the `tracked` / `untracked` cases, and do
not "fix" it by making the fixture's ignored file untracked instead. The tracked
and untracked cases are hard acceptance criteria and must stay exactly as
strict — including under `--force`.

## Scope

`Refusal::Dirty`'s Display string is `"uncommitted, untracked or ignored files
are present"` (`merge_cleanup.rs:144`). It will no longer be accurate for the
admission path; the issue also asks for the outcome to stay **visible** in the
output — `swept … (discarded build state)` vs `kept … — edited since landing`.
An operator needs to see which worktrees lost a warm `target/`, because that is
a real cost even when it is the right call.

`Refusal` is a two-variant enum (`Dirty`, `Unsafe(String)`). A third outcome
almost certainly wants representing in the type rather than inferred at the call
site — but check every match site before you widen it.

In scope: the predicate split, the `Verified` token change if you take that
route, the reporting distinction, `Refusal`/Display as needed, and tests.

Out of scope, do **not** touch:

- **The grace period / TTL logic itself.** `--force` must keep bypassing only
  the grace period and never real work. Do not make `--force` able to discard
  tracked or untracked-non-ignored content.
- The THE-685 filter-driver logic that just landed (`configured_filter_drivers`
  and the `:(attr:filter=<driver>)` pathspec check). It is correct and tested;
  leave it alone.
- The submodule and `skip-worktree`/`assume-unchanged` refusals — unrelated
  safety guards, and both are `Unsafe`, not `Dirty`.
- **The third cause in this chain**, `landed commit identity is missing`
  (affects `tg/spark-radar`, `tg/bold-petal`, `tg/keen-marble`, `tg/bold-mango`).
  It is a separate defect and a separate issue. If you learn something about it
  while you are in here, **report it as a note**; do not fix it.

## Acceptance criteria (from the issue — all six)

- [ ] Ignored-only merged worktree **is** swept once its TTL has elapsed.
- [ ] Any tracked modification ⇒ never swept, with or without `--force`.
- [ ] Any untracked non-ignored file ⇒ never swept.
- [ ] Grace period still applies; `--force` bypasses only the grace period.
- [ ] Output distinguishes "swept, discarded ignored state" from "kept, edited".
- [ ] Tests cover ignored-only, tracked-modified, untracked-non-ignored, and
      clean — **each with and without `--force`**.

The last one is a matrix, not four tests. Do not report the row finished with
the `--force` half missing.

## Testing traps in this file, measured

- **`TestIsolation` mutates process-wide env, so these tests only isolate under
  nextest** (a process per test). Under threaded `cargo test` they interfere:
  the primary saw `gate_runner::unused_configured_filters_are_preserved_without_execution`
  fail under `cargo test` and pass under `cargo nextest run`. If you are told a
  test fails, check which runner produced that.
- **`Fixture::probe()` verifies the branch is merged into main.** Committing on
  the feature branch inside a fixture makes it _unmerged_, and the refusal you
  then get is not the one you were testing. This cost the primary a full
  round-trip on THE-685.
- Fixture git invocations need `-c commit.gpgsign=false`; this repo has global
  signing on and an unconfigured fixture hangs for 120s instead of failing.

## Line numbers

Citations above were read from current `main` by the primary today, not from the
stale audit commit — they should be accurate. Re-verify anyway and say so if
anything has moved.

## Cargo

Attempt `nix develop --command cargo check -p thegn-host --all-targets` and a
narrow `cargo nextest run -p thegn-host merge_cleanup`. **The pipeline sandbox
mounts `/nix/store` read-only and this usually fails outright.** If it does,
say exactly that in your report and stop — the primary runs all Rust
validation centrally and will not hold your row against you for it. Never
report `implementation-ready` for code you could not compile: say what you
could not run.
