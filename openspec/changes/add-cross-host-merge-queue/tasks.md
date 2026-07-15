# Tasks

## 1. Self-describing rows (schema)

- [x] 1.1 Add `location` to the `merge_queue` table (base CREATE + additive
      migration, `SCHEMA_VERSION` bump to 44) and to `MergeQueueRow`.
- [x] 1.2 Populate `location` in `enqueue_merge` (correlated subquery from
      `worktrees.location`); read it in `list_merge_queue`.

## 2. Host-independent membership

- [x] 2.1 Resolve `merge_driver::rows_for_repo` membership from the DB
      (`repo_root_for`), falling back to a local `main_checkout` only for an
      unregistered worktree.
- [x] 2.2 Add `merge_ops::target_loc` (repo root → `GitLoc`).

## 3. Cross-host tip ingest

- [x] 3.1 New `merge_remote` module: `ensure_tip_in_target` — bundle a branch on
      its host and fetch it into the target store under `refs/thegn/mq/<branch>`;
      no-op (returns `refs/heads/<branch>`) for a same-store branch.
- [x] 3.2 `attempt_land` takes the branch's `GitLoc`, ingests when off-host, and
      folds the resulting ref; add `AttemptOutcome::Unreachable`; `drive_queue` and
      the land surfaces defer on it with the reason.

## 4. Target-host anchoring + UI

- [x] 4.1 `merge_ops::remote_target_guard` + guards in `merge drain/land`,
      `land`, `integrate`, and the in-app drain.
- [x] 4.2 Merge-queue panel `@host` chip; help page cross-host section.

## 5. Tests + validation

- [x] 5.1 Unit tests: bundle→fetch across two separate stores; `needs_ingest` /
      same-store no-op; existing local fold/land tests stay green.
- [ ] 5.2 Run `just ci` before opening the PR.
