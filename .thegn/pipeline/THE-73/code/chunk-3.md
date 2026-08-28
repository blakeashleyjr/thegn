# THE-73 · chunk 3 — adoption keys on repo identity, not on a repo-path string

**Read first:** `.thegn/pipeline/THE-73/architect/design.md` §1 ("The adoption
predicate that drops it") and §2/F3. `CLAUDE.md` (dev-loop policy, invariants,
ratchets) is binding.

## Files touched (exact)

- `crates/thegn-host/src/session.rs` (including its inline `mod tests`, line 847)
- `crates/thegn-host/src/run.rs` — **call sites only** (one call at line 2042;
  see step 2). `run.rs` is a ratchet-pinned god-file: add no logic to it.

**Nothing else.** Do not touch `sidebar.rs` (chunk 1) or `hydrate.rs` /
`hydrate_tests.rs` (chunk 2).

## Overlap / dependency

None. File-disjoint from chunks 1 and 2. **Runs in parallel with both.**

## The defect

`crates/thegn-host/src/session.rs:376-409`, inside `resurrect_with_cfg`:

```rust
let adopt = (wt.session_name == session && !known(&worktrees))            // 398
    || (wt.repo_root == session                                           // 399
        && wt.tab_name.starts_with(&format!("{slug}/"))                   // 400
        && !known(&worktrees));                                           // 401
```

Arm 1 is dead in practice: `put_worktree` stamps `session_name = session()`
(`crates/thegn-core/src/db_workspace.rs:267`), which is the literal `"default"`
for every registry row on this machine, never the workspace path. So adoption
rests entirely on arm 2 — and arm 2 additionally requires `wt.repo_root` to
**byte-match** the workspace path string. A row registered by a process that
resolved the repo root differently (a different `$HOME`, a symlinked checkout, a
differently-normalised path) fails that compare and is never adopted, even
though its `{slug}/…` tab prefix already proves it belongs to this workspace.

The prefix is sufficient on its own: `slug` is
`repo::repo_slug_with(db, session_path)` (`session.rs:361`), and
`db.slug_for_repo` assigns one globally-unique slug per repo-root string
(`crates/thegn-core/src/repo.rs:76-104`, table `repo_slugs`). The extra
`repo_root` conjunct can therefore only ever _lose_ rows.

Second, smaller defect in the same family: `switch_to_workspace_deferred`
(`session.rs:712-762`) calls `Session::resurrect(db, repo_path)` at line 727 —
the **back-compat shim** whose own doc (`session.rs:314-321`) names "workspace
switch" as a caller and substitutes `Config::default()`. With an empty
`cfg.env`, `row_is_remote_effective` cannot recognise a non-local placement, so
on the workspace-switch path even a genuinely remote worktree is classified
local and skipped by the `is_dir` check at line 389. That is the `row_is_remote`
fix re-opened on exactly the path this issue's repro clicks.

## Approach

### 1. Drop the `repo_root` byte-compare (the primary fix)

In `resurrect_with_cfg`, arm 2 becomes the slug-prefix test plus `!known(...)`.
Arm 1 stays untouched so nothing that adopts today stops adopting — this is a
strict widening.

Rewrite the surrounding comment (lines 393-401). It currently says rows are
adopted "regardless of the (possibly legacy) `session_name`"; it must now also
say why the `repo_root` string is **not** consulted: the canonical slug is the
repo's identity, and a worktree registered by a process that resolved the root to
a different string (different `$HOME`, symlinked checkout) is still this
workspace's worktree — git and the slug say so, the recorded path string is
bookkeeping. Cite THE-73.

### 2. Stop the workspace switch from using the default-config shim

- `switch_to_workspace_deferred` (`session.rs:712`) gains
  `cfg: &thegn_core::config::Config` and calls `Session::resurrect_with_cfg(db,
repo_path, cfg)` at line 727.
- `switch_to_workspace` (`session.rs:692`) gains the same parameter and passes
  it through (line 695).
- Update the callers. In `crates/thegn-host/src/run.rs` the deferred one is at
  line 2042 inside `switch_workspace` (`run.rs:1966`), which already has a
  `cfg`-bearing context available to its caller — if `switch_workspace` does not
  itself take a `&Config`, add that parameter and thread it from its call sites
  (`handlers/sidebar_activate.rs` passes `cfg` already, so this is mechanical).
  If threading it turns out to need more than adding parameters and updating
  call sites — i.e. some caller genuinely has no config in scope — **stop**,
  keep step 1, leave line 727 as-is, and record the shim as a follow-up in your
  report. Do not restructure `run.rs` to make it fit.
- Also update `Session::resurrect`'s doc comment (`session.rs:313-321`) to drop
  "workspace switch" from the list of shim callers once it no longer is one.

### Guardrails

- Both changes are **pure** — no new I/O, no git, no subprocess. `session.rs`'s
  resurrect already does DB reads; add none.
- No new logic in `run.rs`; parameter/call-site edits only.
- No colour/glyph literal, no `#[cfg]` outside `platform/`, no ignored `Result`
  without a `// best-effort: <why>`. **Do not add an entry to any
  `test/*-ratchet.txt`.**
- Strict widening: every existing `session.rs` test must pass **unedited** (in
  particular `resurrect_normalizes_legacy_home_prefix_and_preserves_active`
  at line 1392, `resurrect_skips_home_rename_that_would_collide` at 1419,
  `resurrect_two_legacy_home_groups_dont_rename_to_same_name` at 1442, and
  `switch_to_workspace_names_home_group_with_canonical_slug` at 1369). If one
  needs editing you have changed pinned behaviour — stop and say so.

## Tests to add

In `crates/thegn-host/src/session.rs`'s `mod tests` (line 847), beside
`resurrect_normalizes_legacy_home_prefix_and_preserves_active`:

1. `resurrect_adopts_a_row_whose_recorded_repo_root_differs` — the THE-73 guard,
   mirroring the `row_is_remote` guard's shape. In-memory DB; register a
   worktree via `db.put_worktree(tab, root, wt, branch, None, None)` where `tab`
   is `"{slug}/foo"` for the workspace's canonical slug but `root` is a
   _different_ string for the same repo (e.g. a `/other-home/...` prefix), and
   the worktree path is **outside any plausible `worktrees_dir`**. Assert
   `resurrect_with_cfg` adopts it as a `WorktreeGroup`. Comment that the row's
   recorded `repo_path` is bookkeeping and the slug is the identity.
2. `resurrect_still_ignores_another_workspaces_row` — a registry row whose
   `tab_name` carries a _different_ slug must **not** be adopted, so widening
   arm 2 did not make adoption promiscuous.
3. `resurrect_does_not_duplicate_an_already_known_group` — the `!known(...)`
   guard still holds when arm 2 now matches more rows.
4. If step 2 lands: a test that `switch_to_workspace_deferred` keeps a worktree
   whose env resolves to a non-local placement in the passed `cfg` while its
   local dir is absent (the shim would drop it). Mirror the fixture style of the
   `row_is_remote_effective` tests in `hydrate_tests.rs:860-900`.

**Test-hermeticity (`CLAUDE.md`):** use `Db::open_memory()` (as the neighbouring
tests do) or isolate `XDG_STATE_HOME`; never touch the real state DB. Any git
fixture needs `-c commit.gpgsign=false`.

## Commands to run (scoped only)

```sh
just quick thegn-host
cargo nextest run -p thegn-host session::
cargo nextest run -p thegn-host resurrect
```

**Do not run** `just test`, `just ci`, `just coverage`, `just lint`, `just e2e`,
or any full-workspace compile.

## Done criteria

- Arm 2 of the adoption predicate no longer byte-compares `wt.repo_root` against
  the session path; the comment explains why, citing THE-73.
- Either the workspace-switch path uses `resurrect_with_cfg` (step 2 landed and
  `Session::resurrect`'s doc updated), **or** your report states plainly that
  step 2 was left out and why.
- New tests pass; every pre-existing `session.rs` test passes unedited.
- `just quick thegn-host` is clean.
- No ratchet file modified; no logic added to `run.rs`.
- Committed with exactly this subject:

  ```
  fix(session): adopt worktrees by repo slug, not by repo-path string (THE-73)
  ```
