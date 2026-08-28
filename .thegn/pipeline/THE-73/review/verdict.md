# THE-73 — security/test/bug review verdict

Branch `tg/the-73-sidebar-reap`, reviewed against
`.thegn/pipeline/THE-73/architect/design.md`, the architect-review verdict
(APPROVED), and the four chunk specs/done-reports (every "Unverified" section
treated as a checklist item). Full branch diff `git diff main...HEAD` read;
**`main` was merged twice during this review** — first (`e235b4c0`, no
conflicts), then again (`0f2c9b73`) after THE-74 (pipeline board + sidebar
pipeline folders) landed mid-review with a real conflict in `sidebar.rs`,
resolved by keeping THE-74's `push_pipeline_group` **and** THE-73's union doc
comment on `gather_groups` (function bodies never conflicted).

PASS

---

## 1. Lead's audit points — each verified

**Union render cannot duplicate or orphan rows.** `gather_groups`
(`sidebar.rs`) collects `live_tabs` + `live_paths` from the session's own-slug
groups (empty paths never key), then fills `home` (tab-name-guarded) and
non-home registry rows (tab-name **or** path-guarded). A registry row whose
group was renamed in-session is caught by the path key; the home row cannot be
renamed (`sidebar_keys.rs:804` refuses), so its tab-name-only guard has no
duplicate window. `repo_path`-empty live-fallback workspaces contribute
nothing (guarded, tested). `next_gi` continues after the live max, so appended
rows sort after their live siblings under Manual. Tests cover: missed row
renders, live row not duplicated, renamed-live-row caught by path, flat-mode
mirror, fallback synthesizes nothing.

**Hit-tables / identity anchors.** `sidebar_view.rs` (the hit machinery) is
untouched by this branch; hits are recomputed from the same build pass the
renderer painted, per frame — no persisted index can drift. Union rows reuse
the pre-existing `{slug}/{label}` pin-key identity (same key a dormant
workspace already used), so pins/marks/menu anchors survive the live↔dormant
flip. THE-74's review fix (mirror leaves get their own pin_key) landed on main
and merged cleanly; THE-74's `lane_targets` map is built independently of
`gather_groups`, so the union does not perturb mirror-leaf targets.

**`row_is_git_listed` fail-safe direction — both halves verified.**
(`hydrate.rs`) Keys on git membership alone — no `worktrees_dir`, no prefix
test anywhere. `git_out → None` (unreadable/absent root), empty root, or empty
worktree all map to **keep**; `None` is cached so a broken root costs one
subprocess per pass. The probe is the _last_ conjunct of the reap branch on
both call sites (hydration `db_worktree_list`, pre-first-frame
`prune_stale_worktree_groups`) — the `||` short-circuit means the steady state
spawns nothing; the doc comment marks the reap-branch-only property as
load-bearing. Properly-removed worktrees (`git worktree remove` / `thegn wt
rm`) still prune: git stops listing + dir gone ⇒ reap. `rm -rf`'d trees stay
(git lists them `prunable`) — deliberate, recoverable-over-destructive,
documented. **The reviewer's accumulation fix (01d8020f) is verified**: the
absolute-guarded `session_root` now gives a group whose registry row was
already deleted a root to ask git about (previously: empty root ⇒ fail-safe ⇒
kept forever); regression test `prune_reaps_a_removed_group_whose_registry_row_is_already_gone`
passes.

**Windows path equality (architect's open check-cross question) — closed by
analysis.** `Path`'s `PartialEq` is component-wise (verified empirically:
`/repo/wt/` == `/repo//wt` is true; a byte compare would be false), and on a
Windows host `\` and `/` are both separators, so mixed-separator registry rows
still compare equal to git's forward-slash output. Trailing slashes and
doubled separators are normalized on every host. No residual reap-unsafe
window.

**No blocking git call on the loop (0% idle).** The render path does no I/O —
`gather_groups` reads the cached hydrate snapshot. The git probe runs only on
the hydration thread and on the pre-first-frame prune's reap branch. The warm
switch's `adopt_missing_registered` does one `db.worktrees()` SELECT plus
`repo_slug_with` (a read when the slug row exists; mints — one INSERT — only
when absent, exactly what the cold arm has always done inline via
`switch_to_workspace_deferred`, which additionally pays a bounded 300 ms
`db_task::flush` and layout writes). This is user-initiated work-in-hand, not
idle work; no new timer, thread, channel, or wake source; `render_plan::plan`
untouched.

**Warm-switch adoption cannot re-adopt another workspace's rows.** The slug is
resolved from an **absolute** session id only (`"default"` / `""` adopt
nothing — tested); `slug_for_repo` mints transactionally and **globally
uniquely** (`proj`, `proj-2`, …), so two same-basename repos never share a
`{slug}/` prefix. Arm 1 (`session_name == session`) is unchanged; the union's
render side keys on the row's recorded `slug` column — the same
registration-time identity the tab_name prefix encodes — so render and
adoption agree. The profile-HOME repro is pinned by
`resurrect_adopts_a_row_whose_recorded_repo_root_differs`.

**Concurrency (hydration thread ↔ resync path).** The hydration thread's reap
writes (`del_worktree`) vs the loop's adoption read: SQLite WAL — readers
never block, no corruption; worst case the loop adopts a row being reaped
(dir missing _and_ git-gone), leaving a stale group until the next startup
prune self-heals. No `Session` is shared across threads; the prune runs
pre-first-frame only.

**Ratchets.** No `test/*-ratchet.txt` entry added or needed: no colour/glyph
literal, no `#[cfg]` outside `platform/`, no `gh` call, no `async fn` in a
provider trait, no new idle poll. New ignored `Result`s are the sanctioned
`let _ = db.del_worktree(...)` cleanup and test-fixture removals. No new
`ACTION_SPECS` action ⇒ no help-page change. No `thegn-core` change ⇒ the 95%
core coverage gate cannot move.

## 2. Scoped verification (post-final-merge, all green)

- `just quick thegn-host` — clean.
- `cargo clippy -p thegn-host --tests` — clean.
- `cargo nextest run -p thegn-host` filters: `sidebar` **248/248** (includes
  THE-74's tests — the merged tree is exercised together), `row_is_git_listed`
  4/4, `prune` 7/7, `resurrect` 12/12, `adopt_missing` 3/3, `warm_switch` 1/1,
  `switch_to_workspace` 2/2.

## 3. Findings (none blocking)

1. **Follow-up (recommend an issue): workspace removal deletes registry rows
   by the recorded `repo_path` string.** `workspace_remove.rs` calls
   `del_worktrees_for_repo(repo_path)` then `del_repo_slug(repo_path)`. Rows
   registered under a _different_ root string (the profile-HOME case this
   branch exists for) survive that delete, and if the freed slug is later
   recycled by a same-basename repo, prefix adoption can pull those orphans
   in. Needs three preconditions (differing recorded root + workspace removal
   - basename reuse + surviving dirs); strictly-wider-but-narrow. Deleting by
     slug prefix (slugs are globally unique) would close it.
2. **Note (accepted): prune's `session_root` precedence.** A session group
   belonging to a _foreign_ repo root is probed against the session repo; a
   false reap additionally requires its dir to be missing. Deliberate trade
   (it is what makes the accumulation fix deterministic) and documented at
   the site.
3. **Lead's pre-push gates own the rest, as the chunks recorded:** e2e
   snapshots will likely need a re-record (the union adds sidebar rows
   wherever a session and registry disagree, and chunk 4 can turn a row into
   a live group + tab — THE-74 moved frames too, so that lane's re-record
   should be coordinated, not doubled); full `just test` / coverage /
   cross-MSRV / doc remain the pre-push gate.
4. **Note (pre-existing):** `git_out` has no timeout; the probe inherits that
   only on the reap branch (dir already missing), where the pre-existing
   `is_dir` stat carried the same hung-mount exposure.

## 4. Verdict

The branch fixes all three chokepoints at the layer the design prescribed,
fails safe in the right direction everywhere git can be wrong, keeps the loop
clean, and its guards are pinned by tests that survived two main merges.
Ready for the merge queue.

PASS
