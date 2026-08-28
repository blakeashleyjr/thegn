# THE-73 · chunk 2 — only git may condemn a worktree row (`row_is_git_listed`)

**Read first:** `.thegn/pipeline/THE-73/architect/design.md` §1 ("The two hard
reaps") and §2/F2. `CLAUDE.md` (dev-loop policy, invariants, ratchets) is
binding.

## Files touched (exact)

- `crates/thegn-host/src/hydrate.rs`
- `crates/thegn-host/src/hydrate_tests.rs`

**Nothing else.** Do not touch `sidebar.rs` (chunk 1) or `session.rs` / `run.rs`
(chunk 3).

## Overlap / dependency

None. File-disjoint from chunks 1 and 3. **Runs in parallel with both.**

## The defect

Two reconcile chokepoints destroy a registry row on the strength of a single
`is_dir` stat, with `row_is_remote_effective` as the only exemption:

- `crates/thegn-host/src/hydrate.rs:1388-1408` — `db_worktree_list` (hydration
  thread): `db.del_worktree(&w.worktree)` + `thegn_core::activity::forget`.
- `crates/thegn-host/src/hydrate.rs:916-933` — `prune_stale_worktree_groups`
  (called from `load_or_seed_session`, `hydrate.rs:1061`): the partition
  predicate at line 922 is
  `g.path.is_empty() || remote.contains(&g.path) || Path::new(&g.path).is_dir()`,
  and the losers are `del_worktree`d.

Neither asks git. "The directory is not readable right now" is not "git no
longer lists this worktree", and only the second claim licenses deleting the
row. Doctrine: git is the source of truth for worktrees.

## Approach

### 1. Add the guard, mirroring `row_is_remote` / `row_is_remote_effective`

Put it in `hydrate.rs` directly **below** `row_is_remote_effective`
(which ends at line 1370), so the three guards read as a family:

```rust
/// Whether git still lists `worktree` as a worktree of `repo_root` — the ONLY
/// evidence that licenses reaping a local registry row.
///
/// Consulted **only on the reap branch**, after the cheap `is_dir` stat and
/// `row_is_remote_effective` have already said "this looks dead". In the steady
/// state (no missing dirs) it spawns nothing, so neither the hydration pass nor
/// the pre-first-frame prune pays for it. Do NOT hoist this onto the happy path.
///
/// `cache` memoises one `git worktree list --porcelain` per repo root for the
/// pass, so N missing rows in one repo cost one subprocess, not N.
///
/// Fail-safe: an unreadable or absent `repo_root` (git_out → None) returns
/// `true`. We could not prove deletion, so we must not destroy the row —
/// the same posture `row_is_remote` takes for an unknown placement.
pub(crate) fn row_is_git_listed(
    repo_root: &str,
    worktree: &str,
    cache: &mut std::collections::HashMap<String, Option<Vec<String>>>,
) -> bool
```

Implementation notes:

- Reuse the existing core seam — **do not** shell out by hand:
  `thegn_core::util::git_out(Path::new(repo_root), &["worktree", "list", "--porcelain"])`
  then `thegn_core::util::parse_worktree_branches(&porc)` (`util.rs:447-465`),
  taking the `.0` path of each pair.
- An empty `repo_root` (or an empty `worktree`) → return `true` (cannot prove
  deletion).
- Compare paths structurally, not by `starts_with`: `Path::new(a) == Path::new(b)`
  (component equality — handles a trailing slash and `//`). **No `worktrees_dir`
  and no prefix test anywhere in this function** — that is the whole point of
  the issue.
- Cache `None` for "git could not be asked" so a broken repo root is probed at
  most once per pass, and that `None` maps to `true` at every call site.

### 2. Wire it into `db_worktree_list` (`hydrate.rs:1388-1408`)

The reap condition becomes: not remote **and** dir missing **and**
`!row_is_git_listed(&w.repo_root, &w.worktree, &mut git_cache)`. Add a
`git_cache` local next to the existing `ambient_cache` (line 1379).

When git _does_ still list it, keep the row and log once at `debug` on
`target: "thegn::hydrate"` (path + tab + repo root, message along the lines of
"registry row kept: dir missing but git still lists this worktree"). Keep the
existing `warn!` on the branch that actually reaps, and extend its message to
say git no longer lists it.

### 3. Wire it into `prune_stale_worktree_groups` (`hydrate.rs:889-945`)

The partition at lines 916-926 has no repo root to hand — `WorktreeGroup` only
carries `name` and `path`. Build a `path → repo_root` map from the same
`db.worktrees()` read that already populates `remote` (lines 896-913) — one
extra `collect`, no extra query — and fall back to
`thegn_core::repo::main_worktree(Path::new(&g.path))` only when the map misses
**and** the group is otherwise about to be reaped (so the fallback, too, is
reap-branch-only).

Keep the group when `row_is_git_listed` says git still knows it. Update the
`tracing::info!` at 938-942 and the function's doc comment (883-888) to state
the new rule: a group is dropped only when its dir is gone **and** git no longer
lists it.

### Guardrails

- **No blocking work on the happy path.** The probe must be unreachable unless a
  row was already about to be deleted. `prune_stale_worktree_groups` runs before
  the first frame (`CLAUDE.md`: no blocking subprocess there), so this
  reap-branch-only property is load-bearing — say so in the comment, and do not
  restructure the predicate in a way that evaluates the probe eagerly (watch out
  for `&&` vs. collecting into a `Vec` of bools).
- No new thread, no new channel, no new wake source. The render decision
  (`render_plan::plan`) is untouched.
- No `thegn-core` change — `git_out` and `parse_worktree_branches` already exist
  there, and the core's 95% coverage gate must not be perturbed.
- No colour/glyph literal, no `#[cfg]` outside `platform/`, no `gh` call, no
  `async fn` in a provider trait. Every `let _ = …` you add needs a
  `// best-effort: <why>`. **Do not add an entry to any `test/*-ratchet.txt`.**

## Tests to add

In `crates/thegn-host/src/hydrate_tests.rs`, **beside the existing
`row_is_remote` / `row_is_remote_effective` guard tests (lines 822-900)** — the
issue explicitly asks for a mirror of those:

1. `row_is_git_listed_is_not_a_worktrees_dir_prefix_test` — the THE-73 guard.
   Build a real temp git repo, `git worktree add` a linked worktree at a path
   **deliberately outside any plausible `worktrees_dir`** (e.g. a second temp
   dir that shares no prefix with the first), and assert `row_is_git_listed`
   returns `true` for it. Comment that this is the local-foreign-dir sibling of
   the `row_is_remote` guard.
2. `row_is_git_listed_is_false_for_a_path_git_never_knew` — same repo, a made-up
   path under it → `false` (so the guard can still authorise a real reap).
3. `row_is_git_listed_fails_safe_when_the_repo_root_is_unreadable` — empty
   `repo_root`, and a nonexistent `repo_root` → both `true`.
4. `row_is_git_listed_probes_each_repo_root_once` — call it twice for two
   different worktrees of the same root and assert the cache has exactly one
   entry.
5. A `prune_stale_worktree_groups` case: a session group whose `path` does not
   exist on disk **but which git still lists** survives and its registry row is
   **not** deleted; and the existing "dir really gone, git doesn't list it"
   behaviour still reaps. If there is already a prune test in the file, mirror
   its fixture style.

**Test-hermeticity rules (`CLAUDE.md`):** anything opening the DB or building a
git fixture must isolate `XDG_STATE_HOME`, and every git fixture must be created
with `-c commit.gpgsign=false` (a fixture inheriting the global git config
otherwise hangs waiting for a signature — see the repo's two fixture
conventions and follow whichever the neighbouring tests use).

## Commands to run (scoped only)

```sh
just quick thegn-host
cargo nextest run -p thegn-host row_is_git_listed
cargo nextest run -p thegn-host prune_stale
cargo nextest run -p thegn-host row_is_remote
```

**Do not run** `just test`, `just ci`, `just coverage`, `just lint`, `just e2e`,
or any full-workspace compile.

## Done criteria

- `row_is_git_listed` exists in `hydrate.rs` next to the `row_is_remote` family,
  is memoised, fails safe, and contains **no** `worktrees_dir` / prefix test.
- Both reaps consult it, on the reap branch only; both log the kept case at
  `debug` and the reaped case at `warn`/`info` with git named as the authority.
- All new tests pass; the existing `row_is_remote*` tests pass unedited.
- `just quick thegn-host` is clean.
- No ratchet file modified; no `thegn-core` file modified.
- Committed with exactly this subject:

  ```
  fix(hydrate): only reap a worktree row git no longer lists (THE-73)
  ```
