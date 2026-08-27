# THE-73 · chunk 2 — done

**Chunk:** only git may condemn a worktree row (`row_is_git_listed`)
**Branch:** `tg/the-73-sidebar-reap`
**Commits:**

- `1f756e0e` — `fix(hydrate): only reap a worktree row git no longer lists (THE-73)`
  (`crates/thegn-host/src/hydrate.rs`, +149/−48) — the spec's mandated subject.
- `51bee7a9` — `test(hydrate): guard tests for row_is_git_listed + the prune reap (THE-73)`
  (`crates/thegn-host/src/hydrate_tests.rs`, +221)

Files touched: exactly the two the spec names. `sidebar.rs` (chunk 1) and
`session.rs` / `run.rs` (chunk 3) untouched; only my own paths were ever staged.

> Ordering note: the spec's exact-subject commit is the **implementation**
> commit, with the test commit stacked on top of it. Siblings committed onto the
> branch between the two, and rebasing was forbidden, so the subject commit is
> not the branch tip.

---

## What landed

### 1. `row_is_git_listed` (`hydrate.rs:1404-1454`)

Placed directly below `row_is_remote_effective`, so the three guards read as a
family.

```rust
pub(crate) fn row_is_git_listed(
    repo_root: &str,
    worktree: &str,
    cache: &mut std::collections::HashMap<String, Option<Vec<String>>>,
) -> bool
```

- Reuses the existing core seam — `thegn_core::util::git_out(…, &["worktree",
"list", "--porcelain"])` + `thegn_core::util::parse_worktree_branches`, taking
  the `.0` of each pair. No hand-rolled subprocess, no new vendor CLI.
- Empty `repo_root` **or** empty `worktree` → `true` (cannot prove deletion).
- Comparison is `Path::new(a) == Path::new(b)` — component equality, so a
  trailing slash and a doubled separator are absorbed. **No `worktrees_dir`, no
  prefix/containment test anywhere in the function.**
- `None` (git unaskable) is cached as `None`, and maps to `true` at every call
  site, so a broken repo root is probed at most once per pass.

### 2. `db_worktree_list` (hydration thread)

The probe is the **last** conjunct, after `row_is_remote_effective` and the
`is_dir` stat:

- git still lists it → row kept, one `debug!` on `thegn::hydrate`
  (`worktree` / `tab` / `repo_root`, "registry row kept: dir missing but git
  still lists this worktree").
- git does not → the pre-existing `warn!` fires, now naming git as the authority
  ("reaping registry row: git no longer lists this worktree, its local dir is
  gone and its env resolves local"), then `del_worktree` + `activity::forget` as
  before.

A `git_cache` local sits beside `ambient_cache`, so N condemned rows in one repo
cost one subprocess.

### 3. `prune_stale_worktree_groups` (pre-first-frame)

- The single `db.worktrees()` read now feeds **two** views: the existing `remote`
  exemption set and a `path → repo_root` map (`HashMap<&str, &str>`, borrowed
  from the same `rows` vec — no extra query, no extra allocation of the strings).
- The partition predicate keeps its `||` chain, with the probe appended as the
  final term:
  `g.path.is_empty() || remote.contains(&g.path) || Path::new(&g.path).is_dir() || { …probe… }`.
  Short-circuiting is what makes the probe reap-branch-only; there is a comment
  at the site saying so and warning against an eager rewrite.
- The `main_worktree` fallback is **inside** that final term, so it too is
  reap-branch-only.
- Kept case logs at `debug!` ("session group kept: dir missing but git still
  lists this worktree"); the reaped case keeps its `tracing::info!`, retitled
  "stale worktrees pruned (dirs gone from disk **and git no longer lists them**)".
- Doc comment rewritten to state the new rule.

---

## Tests added (`hydrate_tests.rs`, immediately after the `row_is_remote*` guards)

A shared fixture, `git_repo_with_linked_worktree(root, linked) -> (root_s,
linked_s)`, builds a real repo + one linked worktree with
`commit.gpgsign=false` / `user.email` / `user.name` set locally, and returns the
paths **as git itself prints them** so a symlinked temp dir can't make the
assertions flap.

1. `row_is_git_listed_is_not_a_worktrees_dir_prefix_test` — the THE-73 guard,
   commented as the local-foreign-dir sibling of `row_is_remote`'s. Linked
   worktree lives under a second temp subtree (`…/somewhere-else-entirely/wt`)
   that shares no prefix with the repo or any plausible `worktrees_dir` → `true`.
   Also covers the main checkout, a trailing slash, a doubled separator, and
   (after `remove_dir_all`) the missing-dir case.
2. `row_is_git_listed_is_false_for_a_path_git_never_knew` → `false`, including
   for a path **inside** the repo tree (membership, not prefix), so the guard can
   still authorise a real reap.
3. `row_is_git_listed_fails_safe_when_the_repo_root_is_unreadable` — empty root,
   absent root, and empty worktree → all `true`.
4. `row_is_git_listed_probes_each_repo_root_once` — two worktrees of one root →
   `cache.len() == 1`; an unaskable root memoises as `None` and is not re-probed.
5. `prune_keeps_a_git_listed_group_whose_dir_is_gone` — two registry rows off the
   same repo (a real linked worktree whose dir is then deleted, and a "ghost"
   path git never knew), both absent from disk. Asserts `pruned == 1`, that the
   git-listed group survives, that its registry row is **not** deleted, and that
   the ghost row still is. Isolates `XDG_STATE_HOME` via
   `crate::testenv::EnvVarGuard`.

---

## Verification

| Command                                             | Result                                                                                   |
| --------------------------------------------------- | ---------------------------------------------------------------------------------------- |
| `just quick thegn-host`                             | clean (no errors, no warnings)                                                           |
| `cargo clippy -p thegn-host --all-targets`          | clean (covers the new test code)                                                         |
| `cargo nextest run -p thegn-host row_is_git_listed` | 4 passed                                                                                 |
| `cargo nextest run -p thegn-host prune_keeps`       | 1 passed                                                                                 |
| `cargo nextest run -p thegn-host hydrate::`         | **42 passed, 0 failed** — includes the two pre-existing `row_is_remote*` tests, unedited |

No full-workspace gate was run (`just test` / `ci` / `coverage` / `lint` / e2e —
all correctly avoided per the dev-loop policy).

Ratchets: no `test/*-ratchet.txt` modified. No `thegn-core` file modified. No new
`let _ = …`, no colour/glyph literal, no `#[cfg]` outside `platform/`, no `gh`
call, no `async fn` in a provider trait, no new thread/channel/wake source, no
`render_plan` change. No new `ACTION_SPECS` action → no help-page change.

---

## Unverified

- **The full test suite / `just lint` / `just ci`.** Scoped runs only, by
  instruction. Notably, coverage (`thegn-core` 95% gate) was not run — but this
  chunk adds no core code, so it cannot move that number.
- **Non-Linux behaviour.** `Path` component equality on Windows (`C:\…` vs
  `C:/…`) was reasoned about, not exercised; `git worktree list --porcelain`
  prints forward slashes there while a registry row may hold backslashes, so the
  structural compare could differ from Linux. `check-cross` was not run.
- **A live end-to-end repro of THE-73** (foreign-dir worktree surviving a
  click-resync). This chunk only removes the _reap_; whether the row then renders
  is chunk 1's `gather_groups` change and chunk 3's adoption change.
- **Interaction with the sibling chunks.** Chunks 1 and 3 both landed on the
  branch while I was working; I ran the hydrate suite against the merged tree
  (green), but not the sidebar/session/run suites.

## Notes for review (deliberate consequences worth a second opinion)

1. **`prunable` entries count as listed.** Verified empirically: after
   `rm -rf <worktree>`, `git worktree list --porcelain` still emits
   `worktree <path>` plus a `prunable …` line, and `parse_worktree_branches`
   only reads `worktree`/`branch`. So a worktree the user deleted with `rm -rf`
   (rather than `thegn wt rm` / `git worktree remove`) is now **never** reaped
   until someone runs `git worktree prune`. That follows the spec's rule
   literally ("only git may condemn"), and arguably matches doctrine — git still
   lists it, so thegn still shows it — but it is a real behaviour change to the
   `db_worktree_list` reap and should be a conscious call, not a side effect.
   Filtering on the `prunable` marker would restore the old behaviour for that
   case; the spec does not ask for it, so I did not.
2. **Prune fail-safe when no repo root is resolvable.** In
   `prune_stale_worktree_groups`, if the `path → repo_root` map misses **and**
   `main_worktree` returns `None` (its argument is the missing dir, so this is
   the common case for a group whose registry row was already deleted), `root` is
   empty → `row_is_git_listed` returns `true` → the group is kept forever. That
   is exactly what the spec prescribes, but it means a session group left over
   from a worktree removed while thegn was **not** running is no longer pruned at
   launch. Mitigating context: `merge_lifecycle::reconcile_removed_tabs` already
   tears down live tabs on the in-app fold path, so the merge-queue
   `on_landed = remove/detach` case named in the function's doc comment is not
   solely dependent on this prune. Flagging it because it is the one place the
   fail-safe posture converts a silent deletion into a silent accumulation.
3. **`row_is_git_listed` returns `true` for an empty `worktree`.** Spec'd, and
   harmless at both call sites (an empty `g.path` is already short-circuited
   earlier in the prune predicate), but worth noting the guard is not symmetric:
   "cannot prove deletion" is the answer to every under-determined input.
