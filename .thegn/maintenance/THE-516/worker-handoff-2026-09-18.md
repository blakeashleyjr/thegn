# THE-516 worker handoff — 2026-09-18 (Claude, after Codex stop)

Status: PARTIAL. Foundation validated; creation/tab/feature doors wired to
collision-free identities. The full issue is NOT complete.

## Foundation validation (chunks 1–2)

- The foundation test target did not compile (db_migrate tests lacked imports
  of private helpers; a usize SQL param). Fixed in d5d26e7d, which also adds the
  revision-4 item-3 regressions: unique-index-failure rollback preserving two
  colliding verified claims + retry refusal, and old-ledger idempotence.
- NOTE: every lane result before the per-worktree target-dir fix is invalid
  (shared CARGO_TARGET_DIR reused other branches' crates). Results below are
  from the per-worktree lane only.

## Wired beyond the foundation (deviations from 477 explained)

- `worktree::worktree_path` REMOVED. Replacements:
  - `worktree_path_for(&RepositoryId, root, branch, cfg)` (pure) and
    `allocate_worktree_path(root, branch, cfg)` (resolves RepositoryId from Git,
    fallible). Leaf = `<slug label>--<full 64-hex sha256(repo id, exact branch
bytes, path mode)>`.
  - Deviation: 477 proposed `<repo-id>/<worktree-id>-<label>` with a random
    generation in the digest. Kept the historical `<worktrees_dir>/<repo name>/`
    parent (tooling walks one level per repo) — same-basename repos may share
    the PARENT but never a checkout because the RepositoryId is in the digest.
    No random nonce: the path is deterministic per (repository, exact branch),
    which keeps retries idempotent; generation stays the Git admin-instance
    stamp from the foundation. Reviewer: challenge this if a
    delete/recreate-at-same-path hazard matters beyond the pre-existence refusal.
  - `provisional_worktree_path` (pure, domain-separated digest) for the
    optimistic wizard tab so a placeholder never aliases a real checkout.
- `add_checked_with_state` refuses any pre-existing destination under the
  mutation lock (`AddError::destination_preexisted`); host
  `worktree_lifecycle::create_failure_after_add` skips rollback for it so a
  refused create never removes someone else's path.
- `rename`: preflight destination, lock, branch -m, move; move failure rolls the
  branch back; failed rollback returns an explicit SPLIT STATE error with the
  recovery command. Not journaled (crash between steps still unrecoverable).
  `worktree_rename::apply` reports registry-update failure instead of silently
  claiming success.
- `repo::branch_tab` is the EXACT branch (`slug/feat/a`), with `home` escaped as
  `home~` (`~` is illegal in refs). Rename already produced exact tabs.
- `repo_slug_checked` / `repo_slug_with_checked`: fail-closed slug for
  wizard, `wt new`, daemon `worktrees.create`, tracker dispatch, autopilot, and
  merge cleanup hook identity. Infallible fallbacks remain for display paths.
- `worktree_for_tab` no longer `LIMIT 1`: >1 claimant is an error.
- Hydration: registry rows sharing `(repo_root, tab_name)` are quarantined —
  warned, stamped `identity_state='quarantined'` (v69 compat column), and not
  surfaced for tab routing. Nothing opened/moved/deleted.
- Features: `project::feature_branch_name` is now literal and fallible
  (`is_valid_branch_name`); no slug normalization, refusal with a hint.
  No schema change / FeatureId table (deviation from chunk 7): exact branch
  equality IS Git's own identity, so no second authority is introduced.

## Still open (acceptance criteria)

- Opaque instance-ID/generation keyed tabs + DB rows (chunk 5): routing is
  still by exact tab string/path; `tab_groups`/caches not re-keyed; ledger
  not populated by creation doors; downstream stale-generation rejection absent.
- Journaled rename + crash recovery at every boundary; DB-failure publication
  CAS (chunk 6); provider/sandbox identity updates on rename.
- Concurrent create serialization by canonical identity beyond the per-repo
  mutation lock; path swap between preflight and add (only lock-held check).
- Legacy admission: background Git inspection that promotes unique legacy rows
  into `worktree_instances` (chunk 8); quarantine surfaced in UI/doctor.
- hydrate.rs no-DB home-tab basename fallback (read-only) unchanged.
- macOS/Windows compile/runtime unverified (Linux lane only).

## Validation (per-worktree lane, after the shared-target fix)

- `cargo clippy --workspace --offline --locked --all-targets -- -D warnings`: clean.
- `cargo nextest run --workspace --offline --locked --no-fail-fast -E 'test(/ratchet/) | test(/help::/) | (package(thegn-core) & (worktree::|repo::|project::|db_tests::|db_worktree_identity|db_workspace|identity::|db_migrate|util::|v69)) | (package(thegn-host) & (worktree_rename|creating|hydrate|merge_lifecycle|tracker|autopilot|wizard|cmd::wt|worktree_lifecycle|daemon::service|session::))'`:
  636 run, 636 passed.
- `nix develop --command treefmt`, `nix develop --command just ratchets`: clean.
- Fixed along the way: two foundation clippy `needless_return`s in util.rs
  (cfg tail blocks) and the core platform-cfg ratchet (test-only unix cfgs in
  repo.rs/worktree.rs pinned with a reason).

## Review round 2 (2026-09-20) — adversarial blockers 1–3 and items 4–7

Rebased onto `main` 18060ae8 (THE-418/454/515 + clippy fix). THE-515 landed at
SCHEMA_VERSION 68, so the v69 bump here is still the only one.

BLOCKER 1 (capture lane): `IDENTITY_CAPTURE_SLOTS` 2 → 16, and acquisition now
WAITS (`IDENTITY_CAPTURE_WAIT`, 10s) on a `Condvar` instead of refusing
instantly, so three concurrent creates queue rather than one being abandoned.
The two `mem::forget((child, budget))` leak paths are replaced by
`withhold_unknown_ownership`: the child handle is forgotten (the unreaped
zombie keeps its PID from being reused, and nothing signals it again) while
its slot is accounted separately and reclaimed after
`IDENTITY_CAPTURE_LEAK_RETENTION` (60s). A leak now degrades throughput for a
minute; it can no longer brick every probe in the process.

BLOCKER 2 (coverage): the lane takes a `&'static IdentityCaptureLane`
parameter (like the existing reader-spawner seam), and the budget regressions
use `test_lane(n)`, so nothing asserts on the process-wide lane. `just coverage`
was then RUN: 4225 core lib tests in one process, gate `core ≥95% lines` green.

BLOCKER 3 (quarantine): ambiguity is now reported, not silent —
`handlers::startup::quarantined_worktree_status` puts a one-line notice in the
launch status, and `thegn doctor` gained a "Worktree identity" section (plus
`worktree_identity` in `--json`). The stamp is RECONCILED, not sticky:
`reconcile_legacy_tab_quarantine` stamps contested rows and clears rows
previously stamped with the same reason once they are no longer contested.
`db_worktree_list_omits_only_contested_rows_and_stamps_them` pins which rows
are withheld, that nothing is deleted, that the stamp lands, and that removing
one claimant brings the other back.

4. `worktree_path_for` takes `repo_dir_name` and is genuinely pure; the Git
   `repo_name` call moved into `allocate_worktree_path` (off-loop).
5. Merge cleanup keeps the path-derived `$THEGN_WORKSPACE` label (signed off,
   reverted): `slug_for_repo` is a write transaction, so a transient
   SQLITE_BUSY would have turned unattended reclaim into a refusal, and the
   hook variable would have silently changed value. No release note needed.
   The same reasoning removed the second `Db::open()` from the wizard and the
   tracker: the authoritative slug is resolved from the handle that registers
   the row (this is also what broke two wizard tests on the first pass).
6. `--program` refusal keeps its hard error, now with the migration path in the
   message (re-run with the literal → attaches to the old slugged branches) and
   a BREAKING entry in CHANGELOG.
7. The pre-existing-destination refusal (add and rename) now names the next
   step (`git worktree list` / `prune`, or pick another name).

Debt also closed: add/rename refuse when `lock_git_mutations` returns None
(previously both proceeded unlocked, so the loser's rollback could destroy the
winner's checkout — `AddError::lock_unavailable`, and
`create_failure_after_add` skips rollback for it); rename validates the new
branch (`is_valid_branch_name`, which now also rejects `HEAD`); the SPLIT
STATE recovery command is shell-quoted; the v69 rebuild refusal prints a
recovery recipe; the porcelain raw-byte test is `#[cfg(unix)]` and its fixture
writes inside the per-test repo instead of /tmp.

Still recorded as debt, not fixed: `verify_v69_schema` remains a hard gate on
every `Db::open` (message is now actionable); the platform-cfg ratchet pins are
file-level, so a future PRODUCTION `#[cfg]` in repo.rs/worktree.rs would pass
silently; `git_admin_instance_stamp` needs `Metadata::created()` (NFS/CIFS/some
FUSE will refuse — a chunk-8 landmine, no production caller today);
`sandbox::container_name` grows ~64 chars against a 64-byte kernel hostname cap;
Windows MAX_PATH headroom for in-repo checkouts.

### Validation (round 2, per-worktree lane)

- focused: 457 run, 457 passed (ratchet tests included).
- `just coverage`: 4225 tests, gate `core ≥95% lines` green.
- `just test` (unfiltered): 8811 run, 8811 passed, 30 skipped.
- `cargo clippy --workspace --all-targets -- -D warnings`: clean.
- `treefmt --ci` and `just ratchets`: clean.
