# Primary review — THE-677 revision 1 (reviewing row 641)

The design is right and the core is proven: **`cargo nextest run -p thegn-core`
on `disk_reclaim` / `config_example` / `env_overlay` is 26/26 green**, and the
pure policy reads correctly — age floor, newest-per-package-name retention,
lexically-greatest tie-break on equal mtimes, duplicate rows ignored. All four
of my decisions are implemented, including the fingerprint-first deletion order
and the non-blocking `.cargo-lock` acquisition.

**Two of your own host tests fail**, and they are the ones that prove the thing
works end to end:

```
measure::disk::generation_tests::unlocked_prune_removes_only_selected_footprint_and_records_real_bytes
  → disk.rs:657  assertion failed: removed > 0
measure::disk::generation_tests::failed_profile_inventory_does_not_suppress_a_different_profile
  → disk.rs:753  assertion failed: removed > 0
```

Both share `profile_tree()`, so it is one cause.

## What the primary has already ruled out

I replicated `profile_tree()` exactly outside the test and checked each gate:

- **`git status --porcelain` is EMPTY** in the fixture, so the dirty-worktree
  guard at `disk.rs:304` does not skip it.
- **No symlinks anywhere on the path** (`readlink -f` is identity), so
  `safe_directory` / `safe_regular_file` accept the profile dir and `.cargo-lock`.
- **`plan_generations` would evict `pkg-a`**: `rsplit_once('-')` groups `pkg-a`
  and `pkg-z` under package `pkg`; their mtimes are equal, so the lexically
  greatest id `z` is retained and `a` is evicted; `u64::MAX.saturating_sub(mtime)
  > = 0`holds. There is no zero-disables rule in the policy, so`min_age_days = 0`
  > is not the problem.

So the plan is correct and the worktree-level guards pass. The eviction is being
lost **between the plan and the unlink**, or the inventory is coming back empty.

## What to check, in this order

1. **`generation_inventory(&profile_dir)`** — is it returning an empty vec or an
   `Err` for this fixture? Note the fixture's artifacts are `deps/libpkg-a.rlib`,
   whose stem is `libpkg-a`, not `pkg-a`. If the inventory associates `deps/`
   artifacts with a fingerprint by splitting the _artifact_ stem, `libpkg-a`
   groups under package `libpkg` and never matches fingerprint `pkg-a`. Either the
   association rule needs the `lib` prefix stripped, or the fixture's filenames
   are wrong — **decide which, and say which you chose.** Real cargo writes
   `libfoo-<hash>.rlib` for package `foo`, so the prefix almost certainly has to be
   handled.
2. **`lock.try_lock()` at `disk.rs:320`** — confirm what this returns on this
   toolchain when the lock IS free. If the API yields `io::Result<bool>`, then
   `.is_err()` is `false` both when the lock was acquired and when it would have
   blocked, so the guard never fires and a **concurrent cargo build would not be
   detected** — which is the whole point of decision 3. Check it and handle the
   not-acquired case explicitly, whatever the shape.
3. **`crate::task::slot_active(worktree_path)`** — verify it is false in a unit
   test with no registered slots, and that it does not touch real state. This test
   does not use `TestIsolation`; if `slot_active` or `git_out` reads
   `XDG_STATE_HOME`, the test is not hermetic and **must** isolate it (this shell
   often runs inside a live thegn, so a test touching the real DB is a real hazard).

Add a temporary diagnostic to find which gate rejects it if that is faster than
reading — just remove it before reporting.

## Required regardless of the cause

- **The two tests must pass, not be weakened.** `removed > 0` is the assertion that
  distinguishes "the policy selected something" from "bytes actually came back",
  and decision 5 asks specifically for _actual_ removed bytes rather than planned.
  Do not relax it to `>= 0`, and do not delete the test.
- If the fixture filenames were wrong, fix the fixture **and** make sure the real
  cargo naming (`lib<pkg>-<hash>.rlib`, `<pkg>-<hash>.d`, executables without the
  `lib` prefix) is what the inventory actually matches. A footprint that fails to
  associate its artifacts would silently prune only the fingerprint directory —
  which is the **dangerous** half-deletion your own finding 3 warned about, in
  reverse: fingerprint gone, artifacts orphaned. Worse, it would look like it
  worked.

## Confirmed — do not change

Deletion order (fingerprint → deps → incremental) with the reasoning comment;
`GenerationPolicy`'s shape and the single `[disk]` age-floor key with its three
ratchets; the separate generation-vs-whole-target byte reporting; the narrowed
retention-only guarantee with no "never removes a live generation" claim.

## Validation

Attempt `nix develop --command cargo nextest run -p thegn-host generation_tests`.
**The pipeline sandbox mounts `/nix/store` read-only, so this usually fails
outright** — say exactly that and stop if it does. The primary re-runs both suites
regardless.

Never report a verdict for code you could not compile or tests you could not run.
