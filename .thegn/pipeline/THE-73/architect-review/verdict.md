# THE-73 — architect review verdict

Branch `tg/the-73-sidebar-reap`, reviewed against
`.thegn/pipeline/THE-73/architect/design.md` and the chunk 1–3 specs/reports.

REVISE

Revision chunk: `.thegn/pipeline/THE-73/code/chunk-4.md`

Chunks 1–3 are correct, faithful to the design, and stay. They close the
_visibility_ half of the reported repro. They leave the _activation_ half open,
and on the user's live machine that is the majority of the rows.

---

## 0. Merge + scope

`git merge main` was a no-op — `main` (`982ab7cb`) is already an ancestor of
this branch, so no conflicts and nothing to resolve. Reviewed the full
`git diff main...HEAD`: 8 source files, +2044/−105 (of which ~1200 lines are the
pipeline docs).

Commit `2d107644` reuses chunk 3's exact subject for the test-only commit, and
`e2d3e256` carries the implementation — **both are coherent**; the pair is the
same split chunk 2 used (`1f756e0e` impl + `51bee7a9` tests), just without the
`test(...)` prefix on the second. The tree is the union of all three, and the
subject-line duplication has no effect on the diff. Not worth a rewrite.

## 1. What was actually losing the rows (the live repro)

The Lead asked which chokepoint was reaping them. **Nothing was reaping them** —
the design's §0 correction is confirmed empirically:

- The live state DB holds **12** `thegn/…` rows in `worktrees`, all with
  `repo_path = /home/blake/code/thegn`, `session_name = default`, dirs present.
- `tab_groups` for session `/home/blake/code/thegn` holds **4** of them
  (`tg-plump-husky`, `tg-the-68-log-noise`, `tg-the-46-weather`,
  `tg-the-36-completions`) plus `thegn/home`.
- `~/.local/state/thegn/logs/*.log` contains **zero** `reaping registry row` and
  zero `stale worktrees pruned` lines. No deletion ever happened.

So the chokepoint is a **render-source flip**, not a reap:
`sidebar.rs::gather_groups` chose the session **or** the registry per workspace
(`let live = !groups.is_empty(); if !live && … { synthesize from db_by_slug }`).
While `thegn` was dormant, all 12 registry rows rendered. Clicking one made the
workspace live, `gather_groups` flipped to the session-only branch, and the 8
rows the live session did not carry vanished. Switching away made it dormant
again and they returned — the exact reported signature, DB untouched throughout.

Why the live session is 8 rows behind is `run.rs::switch_workspace`'s **warm
arm**: `pool.contains(target)` restores the parked in-memory tree verbatim, with
no DB read and no adoption, so worktrees registered by `thegn wt new` from
another shell while the workspace was parked never become live groups. (A cold
start does re-read the registry, which is why a restart "fixes" it.)

**That chokepoint is closed.** Chunk 1 (F1) deletes the `!live` gate and emits
the union — live groups first, then every registry row of this slug the session
does not already carry, deduped by `tab_name` and then by `path`. Whether the
session adopted a row, and whether the switch was warm or cold, no longer
decides whether it renders. The 8 rows render again.

Note the corollary: the differing-`$HOME` / differing-`repo_path` mechanism the
issue's title suggests is **not** what fired here (the recorded `repo_path` does
byte-match, so chunk 3's arm-2 widening was not load-bearing for this repro).
Chunk 3 is still right — it removes a real adoption loss for symlinked/
foreign-root registrations — it just was not the cause on this machine.

## 2. The gap — REVISE

F1's union rows carry `RowTarget::Workspace { repo_path, group: Some(tab) }`,
the target the dormant branch has always used. For a dormant workspace that is a
real switch. For the **active** workspace — a placement F1 created for the first
time — it is a dead click:

```rust
// run.rs:1988, switch_workspace
if session.id == target {
    land_on(session, group);   // no-op when no group has that name
    return true;               // ...and reports success
}
```

`sidebar_activate.rs` only surfaces a status when `switch_workspace` returns
`false`, so activating one of these rows does **literally nothing** — no pane,
no switch, no message. The call site's own comment calls that "the one outcome
the user can't diagnose."

On the live machine this is not an edge case: after chunks 1–3, 8 of the 12
`thegn` worktrees render and none of them opens while `thegn` is the active
workspace, and every warm switch re-parks the same stale tree. The design
anticipated the degradation ("row is not focus-clickable in-place") but assumed
it would be a rare fallback for an adoption miss, not the steady state produced
by the warm pool.

`chunk-4.md` closes it: extract the adoption predicate out of
`resurrect_with_cfg` into a `Session` method, call it on the warm restore (so
the warm arm sees the same registry the cold arm does), retry `land_on` after
adopting on the same-workspace path, and give the residual case an accurate
status. Append-only, no re-sort — `session.active` is an index.

## 3. Fixes applied in this review (commit `01d8020f`)

1. **`prune_stale_worktree_groups` could not ask git about a group whose
   registry row was already gone.** The `path → repo_root` map missed and
   `main_worktree`'s argument is the dir that vanished (so it returns `None`),
   leaving `row_is_git_listed` with an empty root — which fails safe and keeps
   the group **forever**. A worktree removed properly (`thegn wt rm` /
   `git worktree remove`) while thegn was not running therefore accumulated
   instead of being pruned: chunk 2's own report flagged this as "the one place
   the fail-safe posture converts a silent deletion into a silent accumulation."
   Every group in a session belongs to that session's workspace, so `session.id`
   _is_ their repo root — and unlike a recorded `repo_path` it is a path this
   process resolved (the same argument chunk 3 makes). Preferred over the
   registry map, guarded on `is_absolute()` so a legacy non-path session name
   ("default") cannot resolve git against the process cwd. New test
   `prune_reaps_a_removed_group_whose_registry_row_is_already_gone`; verified
   non-vacuous (without the change `main_worktree` → `None` → root `""` → kept).
2. **Documented the `prunable` trade** in `row_is_git_listed`'s doc comment —
   see §4.
3. **`clippy::type_complexity` in chunk 1's new test fixture.** Test targets were
   never linted for chunk 1, and `foreign_dir_registry_row`'s return type trips
   the lint — which fails the `-D warnings` pre-push gate. Named the workspace
   tuple (`type WsRow`).

## 4. Accepted with a flag — the `prunable` consequence

Verified empirically: after `rm -rf <worktree>`, `git worktree list --porcelain`
still prints `worktree <path>` plus a `prunable …` line, and
`parse_worktree_branches` reads only `worktree`/`branch`. So a worktree deleted
outside git is **never** reaped until someone runs `git worktree prune`, and the
local reap now fires only for rows git genuinely dropped (`git worktree remove`,
`prune`) or never knew.

Chunk 2 raised this as a conscious call; I am confirming it as **correct as
designed**. The same "dir isn't there" signal also fires for a transiently
unreadable tree — an unmounted sshfs/autofs path, a profile home that briefly
vanished — which design §1(b) names explicitly, and keeping a ghost row visible
is recoverable where deleting a live one is not. Reap-on-`prunable` would
restore the old behaviour for the first case at the cost of the second. The
trade is now written into the doc comment so a future reader has to decide it
again rather than "fix" it by accident.

## 5. Verified, and what remains unverified

Chunk reports' "Unverified" sections, resolved:

| Claim                                                                             | Verdict                                                                                                                                                                                                                                                                                                                                                                                               |
| --------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Chunk 1: `just quick` doesn't lint test targets                                   | **Real, and it mattered** — `cargo clippy -p thegn-host --tests` found the `type_complexity` warning. Fixed.                                                                                                                                                                                                                                                                                          |
| Chunk 1/2/3: chunks 2 and 3 ran concurrently, combined behaviour untested         | **Now tested.** `cargo nextest run -p thegn-host` over `sidebar\|hydrate\|session\|resurrect\|prune\|switch`: **387 passed, 0 failed** (386 before my new test). No pre-existing assertion was edited by any chunk.                                                                                                                                                                                   |
| Chunk 1/2/3: full-workspace gates not run                                         | Still true, by budget. `just quick thegn-host` + `cargo clippy -p thegn-host --tests` are clean. `just test` / `just lint` / `just coverage` / cross / MSRV remain the Lead's pre-push gate. Coverage cannot have moved: **no `thegn-core` file is touched** by the branch (confirmed against the diff), and the 95% gate is core-only.                                                               |
| Chunk 2: `Path` component equality on Windows                                     | Still unverified; `check-cross` not run. `git worktree list --porcelain` prints forward slashes on Windows while a registry row may hold backslashes, so `Path::new(a) == Path::new(b)` could differ there. Low risk (the guard fails safe toward _keeping_ rows) but it is a real cross-platform question for the Lead's `check-cross`.                                                              |
| Chunk 3: the six `run.rs` `switch_workspace` call sites are compile-verified only | Confirmed — they are mechanical `keymap.config()` additions, and no test drives those in-loop arms. Chunk 4's tests will exercise `switch_workspace` directly.                                                                                                                                                                                                                                        |
| Chunk 3: `switch_to_workspace` (sync wrapper) still passes `Config::default()`    | **Accepted deviation**, correctly taken under the spec's escape clause. Threading a real config means changing `land_after_workspace_removed` → `remove_workspace` → its handler callers, plus `workspace_create::resolve_or_create` — 5+ files. Its callers are workspace create/remove, not the repro path, and F1 keeps the rows visible either way. Recorded as a follow-up below, not a blocker. |

Ratchets and invariants: **no `test/*-ratchet.txt` modified** (`git diff
main...HEAD --name-only` matches nothing under `test/`); no `thegn-core` file
touched; no new `ACTION_SPECS` action/keybind/zone/panel section so no
`docs/help/` change; `render_plan::plan` untouched; no new thread, channel or
wake source. `gather_groups` stayed pure. The git probe is reap-branch-only in
both call sites — the `||` short-circuit in the prune predicate is what makes
that true before the first frame, and both the code comment and the design say
so.

**Frame-affecting change, e2e not run** (known broken, per budget): chunk 1 can
add sidebar rows for a live workspace whose registry holds worktrees the session
missed — on this machine, 8 of them. If `test/muse/snapshots/` has any fixture
whose session and registry disagree, those baselines will need a re-record.

## 6. Follow-ups (not blockers)

1. Give `Session::switch_to_workspace` a real config (chunk 3's recorded
   follow-up) — workspace create/remove still resurrect through the
   default-config shim, so a remote-placement worktree can still be dropped from
   the _session_ on those two paths.
2. `repo_slugs` in the live DB contains a slug minted for every **worktree**
   path (`/home/blake/.superzej/worktrees/thegn/tg-…` → `tg-…`), i.e. something
   is calling `repo_slug_with` with a worktree path as if it were a repo root.
   Harmless today, but `slug_for_repo` _writes_ on that read path, and chunk 3
   made the slug prefix the sole adoption key — so a mis-minted slug is now
   load-bearing. Worth its own issue.
3. The same DB shows `nix-shell.*/sz-wiz-*` slugs — test runs are writing to the
   **real** state DB, which is the known `wt new`/hermeticity bug. Unrelated to
   this branch.
