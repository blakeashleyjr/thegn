# THE-73 — architect review verdict (re-review after chunk 4)

Branch `tg/the-73-sidebar-reap`, reviewed against
`.thegn/pipeline/THE-73/architect/design.md` and the chunk 1–4 specs/reports.
This supersedes the REVISE verdict that raised chunk 4.

APPROVED

Chunk 4 closes the activation gap I described. Both halves of the reported
repro — the row disappearing, and (after chunk 1) the row rendering but not
opening — are now closed, and the residual case says why instead of doing
nothing.

---

## 0. Merge + scope

`git merge main` was a no-op again — `main` (`982ab7cb`) is still an ancestor of
this branch, so there was nothing to resolve. Reviewed the full
`git diff main...HEAD`: 7 source files, +3153/−144 (of which ~1600 lines are the
pipeline docs).

New since the previous verdict (`c79289f1`):

- `0ae915b6` — chunk 4 implementation
- `04ecade7` — chunk 4 tests
- `0dd2a57e` — chunk-4 completion summary

Note for the Lead: there is no commit with the subject
`test(session): pin warm-switch registry adoption and same-workspace landing
(THE-73)`. The test commit carries the implementation subject verbatim (the
chunk-4 spec pinned that exact subject and the coder applied it to both halves
of the split, as chunk 3 did). Coherent; the tree is the union; not worth a
rewrite.

## 1. The live repro, end to end — which chokepoint, and that it is closed

The Lead asked which chokepoint was reaping the rows. **Nothing was reaping
them.** Re-confirmed against the live state DB and logs today:

| Evidence                                                                          | Value                                                              |
| --------------------------------------------------------------------------------- | ------------------------------------------------------------------ |
| `repo_slugs` slug for `/home/blake/code/thegn`                                    | `thegn`                                                            |
| `worktrees` rows with `repo_path = /home/blake/code/thegn`                        | **14**, every `tab_name` prefixed `thegn/`, `session_name=default` |
| `tab_groups` rows for `session_name = /home/blake/code/thegn`                     | `thegn/home` + **4** `thegn/…` groups (plus 2 foreign groups)      |
| `reaping registry row` / `stale worktrees pruned` in `~/.local/state/thegn/logs/` | **zero**                                                           |

So the DB never lost anything: the persisted session layout is simply ten rows
behind the registry. The chokepoint is a **render-source flip plus a
non-adopting warm switch**, exactly as design §0 predicted:

1. **`sidebar.rs::gather_groups`** rendered the session **or** the registry per
   workspace — `let live = !groups.is_empty();` then
   `if !live && … { synthesize from db_by_slug }`. While `thegn` was dormant,
   all 14 registry rows rendered.
2. The click's target is `RowTarget::Workspace { repo_path, group }`
   (`handlers/sidebar_activate.rs:92`) → `run.rs::switch_workspace`.
3. `thegn` was resident in the pool, so the **warm arm** fired: it restored the
   parked in-memory tree verbatim — no DB read, no adoption — leaving the 5
   groups the layout knew.
4. The workspace was now live, `gather_groups` flipped to the session-only
   branch, and the 10 registry-only rows vanished. Switching away made it
   dormant again and they returned. DB untouched throughout — the exact
   reported signature.

The differing-`$HOME` mechanism the issue title suggests is **not** what fired:
the rows' recorded `repo_path` does byte-match `/home/blake/code/thegn`, because
`thegn wt new` resolved the same root under the profile HOME. Chunk 3's arm-2
widening is still right (it removes a real adoption loss for symlinked /
foreign-root registrations) — it just was not load-bearing here.

**Both links are now closed:**

- **Rendering** — chunk 1 (F1) drops the `!live` gate and emits the union: live
  groups first, then every registry row of this slug the session does not
  already carry (deduped by `tab_name`, then by `path`). Whether the session
  adopted a row no longer decides whether it renders. All 14 render.
- **Activation** — chunk 4. Traced end to end on the current tree:
  - **Warm arm** (`run.rs:2030`): `session.adopt_missing_registered(db, cfg)`
    runs after `session.id = target` / `session.worktrees = rw.worktrees` and
    **before** `land_on`, so the restored tree is topped up from the registry
    and the land lands. Slug resolves to `thegn`, all 14 rows carry the
    `thegn/` prefix, all dirs exist → the 10 missing become live groups.
  - **Same-workspace arm** (`run.rs:1987`): `land_on_group(name)`; on a miss,
    adopt and retry. Previously this arm dropped the click on the floor and
    returned `true`.
  - **Cold arm**: unchanged — `switch_to_workspace_tab` /
    `switch_to_workspace_deferred` already resurrect through
    `resurrect_with_cfg`, which calls the same extracted adoption function.
  - **Residual** (`handlers/sidebar_activate.rs:127`): after a `true` return,
    if the named group still isn't in the session, `model.status` becomes
    `'{name}' isn't loaded — its registry row may be stale`. Verified nothing
    downstream in `activate_row_target` overwrites `model.status`, and that the
    existing "gone or unreadable" message (which would be false here) is not
    reused.
  - **The adopted group opens.** A freshly adopted group is `Tab::new` →
    `CenterTree::Leaf(0)`; pane ids start at 1, so leaf 0 always reads as a
    missing leaf and the loop's unconditional lazy-materialize step
    (`run.rs:7953-7990`, `panes.missing_leaves` → `maybe_materialize`) spawns
    its shell on the next turn. That is the same shape a cold-start adopted row
    has always had, so no new path.

## 2. Chunk 4 against its spec

| Spec requirement                                                | Verdict                                                                                                                                                                                                                          |
| --------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Adoption predicate extracted, called by both arms               | Done — `session.rs::adopt_registered_worktrees`; `resurrect_with_cfg` is now a one-line call and keeps its three-tier sort.                                                                                                      |
| Pure extraction (no pre-existing test edited)                   | Confirmed. The only deletions in `c79289f1..HEAD` under `crates/` are the moved block and the one `land_on(session, group);` line. All 19 pre-existing resurrect/switch tests pass unedited.                                     |
| Append-only, no re-sort (`active` is an index)                  | Confirmed — `adopt_missing_registered` drops the returned `positions` map and never sorts. `db.worktrees()` is `ORDER BY position, created_at, worktree`, so the appended tail is in registry-position order, as the spec asked. |
| `is_absolute()` guard on a non-path session id                  | Present, and tested for both `"default"` and `""`.                                                                                                                                                                               |
| No logic in `run.rs`                                            | Held: +22/−1 there, 16 lines comment, 5 lines of calls.                                                                                                                                                                          |
| Accurate residual status, `switch_workspace`'s return unchanged | Held.                                                                                                                                                                                                                            |
| Strict widening                                                 | Held — `land_on_group` is `land_on`'s body plus a bool; behaviour for `group == None` and for an already-present group is byte-identical.                                                                                        |
| No ratchet / no `thegn-core` / no `docs/help` change            | Confirmed: `git diff main...HEAD --name-only` touches nothing under `test/`, `crates/thegn-core/` or `docs/`.                                                                                                                    |

## 3. Tests re-run (scoped, per budget)

```
cargo nextest run -p thegn-host \
  -E 'test(session::) + test(resurrect) + test(adopt) + test(switch) + test(the73)
      + test(prune) + test(sidebar)'
  → 305 tests run: 305 passed, 0 failed

cargo nextest run -p thegn-host \
  -E 'test(adopt_missing_registered) + test(a_warm_switch_adopts)
      + test(activating_a_not_yet_loaded) + test(resurrect) + test(the73)
      + test(prune_reaps) + test(row_is_git_listed) + test(foreign_dir)
      + test(dormant_workspace) + test(switch_to_workspace)'
  → 29 tests run: 29 passed, 0 failed

cargo clippy -p thegn-host --tests      → clean, no warnings (the `-D warnings`
                                          pre-push gate would pass)
```

The second run is the THE-73 set specifically: chunk 2's four
`row_is_git_listed` guards, my `prune_reaps_a_removed_group_whose_registry_row_is_already_gone`,
chunk 3's four resurrect guards, chunk 4's three `adopt_missing_registered`
tests and both `run::tests` switch guards, plus the four
`dormant_workspace_*` sidebar guards chunk 1 mirrors. All green.

Chunk 4's non-vacuity claim (both `run_tests.rs` guards fail with the two
`run.rs` calls reverted) is consistent with the code — the same-workspace guard
can only pass through `adopt_missing_registered`, since the fixture session
carries `home` only.

## 4. Accepted with a flag

1. **The warm arm now does synchronous DB work on the event loop.** One
   `db.worktrees()` SELECT, one `slug_for_repo` (which may _insert_), one
   `is_dir` stat per row, and at most one ambient-env lookup per distinct
   `repo_root`. A warm switch previously did none of that. This is sanctioned by
   the chunk spec and is proportionate — it is a user-initiated event, and the
   cold arm already pays a full resurrect plus a 300 ms `db_task::flush`
   barrier — but it is a real change to the switch's work shape and it is
   argued, not measured (`just bench` is machine-dependent and out of budget).
   The comment at the call site says so. Not idle work, no new wake source, no
   new thread/channel, `render_plan::plan` untouched.
2. **Adopted groups are not written to `tab_groups` at adoption time.** The
   same-workspace arm takes the early return and the handler's `structural`
   flag stays false, so only `persist_active_focus` runs; the warm arm persists
   the _outgoing_ layout, not the incoming one. Self-healing rather than lossy:
   the registry row is the durable record and the next resurrect re-adopts it,
   and the next switch away persists the tree. Left alone deliberately — forcing
   a `persist_session_layout` here would put a full layout rewrite on the
   activation path for no durability gain.
3. **Adoption tops up but never prunes.** A group whose worktree was removed
   from another shell while the workspace was parked survives the warm restore.
   That is caught downstream by the loop's active-group `dir_missing` check
   (`run.rs:7921`), which prunes it, deletes the registry row and says so.
4. **The `prunable` trade** (chunk 2 / previous verdict §4) is unchanged and
   still documented in `row_is_git_listed`'s doc comment: a worktree `rm -rf`'d
   outside git stays listed as `prunable` and so is never reaped until someone
   runs `git worktree prune`. Correct as designed — the same "dir isn't there"
   signal fires for a transiently unreadable tree, and a ghost row is
   recoverable where a deleted live one is not.

## 5. Still unverified (Lead's pre-push gate owns these)

- **Full-workspace gates not run**, per budget: `just test`, `just lint`,
  `just coverage`, cross / MSRV / doc. `just quick`-equivalent scoped clippy
  (`-p thegn-host --tests`) is clean. Coverage cannot have moved — the 95% gate
  is `thegn-core`-only and no core file is touched.
- **Windows path equality in `row_is_git_listed`** (chunk 2's open question):
  `git worktree list --porcelain` prints forward slashes while a registry row
  may hold backslashes, so `Path::new(a) == Path::new(b)` could differ there.
  Low risk — the guard fails safe toward _keeping_ rows — but it is a genuine
  `check-cross` question.
- **Frame-affecting, e2e not run** (known broken, per budget). Two ways frames
  move: chunk 1 adds sidebar rows for a live workspace whose registry holds
  worktrees the session missed (on this machine, ten of them), and chunk 4 can
  turn such a row into a live group + tab on activation. Any
  `test/muse/snapshots/` fixture whose session and registry disagree will need
  a re-record.
- **`adopt_missing_registered_ignores_a_non_path_session_id`'s `"default"` half
  is env-sensitive** (vacuous if `THEGN_SESSION` is set to something else); the
  `""` half is unconditional. Fine as-is.

## 6. Follow-ups (not blockers, carried forward)

1. `Session::switch_to_workspace` (the sync wrapper) still resurrects through
   the `Config::default()` shim — workspace create/remove can still drop a
   remote-placement worktree from the _session_ on those two paths. Chunk 3
   recorded this under its spec's escape clause; threading a real config touches
   5+ files.
2. `repo_slugs` in the live DB holds a slug minted for every **worktree** path
   (`…/worktrees/thegn/tg-…` → `tg-…`), i.e. something calls `repo_slug_with`
   with a worktree path as if it were a repo root. `slug_for_repo` _writes_ on
   that read path, and chunk 3 made the slug prefix the sole adoption key — so
   a mis-minted slug is now load-bearing. Worth its own issue.
3. The same DB shows `nix-shell.*/sz-wiz-*` slugs — test runs writing to the
   **real** state DB (the known `wt new` hermeticity bug). Unrelated to this
   branch; it is also why chunk 4's `run_tests.rs` warm-switch test has to hold
   `ENV_LOCK` and redirect `XDG_STATE_HOME`.
