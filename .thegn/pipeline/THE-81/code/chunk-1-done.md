# THE-81 chunk 1 — thegn-core ignored-result audit: completion report

Branch `tg/the-81-ignored-result-audit`, code commit
`51fac638` — `fix(the-81): audit thegn-core ignored-results — annotate best-effort, surface primary-path errors`.
Scope per `chunk-1.md`: all 84 pinned files under `crates/thegn-core/` (603
matching sites at base `d361b60a`), plus surgical deletions in
`test/ignored-result-ratchet.txt`. Rubric and containment rules from
`.thegn/pipeline/THE-81/architect/design.md` were followed: annotation (a)
is comments-only; (b) handling is limited to the site itself (no signature
changes, no new unwraps); (c) sites are untouched and listed below.

## Result summary

- **603 sites audited** across 84 files (one extra `git grep -c` hit at base
  was a comment-only line, which the gate ignores).
- **(a) annotated this chunk: 501** — `// best-effort: <why>` added (trailing
  on one-line lets, on the line above otherwise). 536 sites are annotated as
  of HEAD: the other **35 already carried best-effort-equivalent prose**
  that names its why (yazi.rs's "best-effort (see `prepare`)" block ×11,
  `heal.rs:29`, `remote.rs:130/381/584`, `remote_tune.rs:51`, `retry.rs:38`,
  `db.rs:883/897/912/962`, `db_workspace.rs:371`, `host_db.rs` ×3,
  `placement.rs:531`, `sandbox.rs:1493`, `sandbox_tests.rs:683/753`,
  `profile.rs:295-296`, `direnv.rs:135`, `util.rs:726`, `activity.rs:897/932`)
  and were left untouched per the design ("do not mass-rewrite existing
  prose").
- **(b) surfaced: 34 sites** — rewritten so the pattern no longer matches:
  - `db_pool.rs` ×4, `db_hibernate.rs` ×1, `db_workspace.rs` ×2
    (`worktree_for_tab`, `get_layout`), `db_iroh.rs` ×1 (`verify_iroh_token`):
    the `.ok() → Ok(Option)` cache-read shape conflated "no row" with a real
    DB error on state the DB is the source of truth for (hibernation records,
    layouts, pool spares, token auth). Rewritten to a match that keeps
    `QueryReturnedNoRows → None` (miss semantics unchanged — zero behaviour
    change) and emits `tracing::warn!(target: "thegn::db", …)` on real errors.
  - `dns_filter.rs` listener spawn: thread-spawn failure now
    `tracing::warn!`s instead of silently skipping the listener.
  - `forge/mod.rs` ×24: `let _ = (loc, branch);` unused-param discards in the
    trait's default `unsupported!` impls → params renamed `_loc`, `_branch`,
    … and the discard lines deleted. (`drop(...)` was rejected: all-Copy
    tuples trip `clippy::drop_copy` under `-D warnings`.)
  - `calendar/recur.rs:679`: `let _ = start;` → param renamed `_start` in
    `week_dates` (only site in the file).
- **(c) flagged for reviewer: 22 sites + 2 class notes** (below).
- **Pattern false-positives (non-Result discards) reported: 11** (below).
- **Allowlist: 325 → 321 pinned** (84 → 80 `thegn-core` lines). Deleted
  exactly the four released files: `calendar/recur.rs`, `db_hibernate.rs`,
  `db_pool.rs`, `forge/mod.rs`. Nothing added; header untouched.

Count check: 603 = 501 (a-ann) + 35 (pre-annotated prose) + 34 (b) + 22 (c)

- 11 (fp). The per-file table below distributes the same 603.

## Per-file table

`sites` = matching sites at base. a-ann = annotated this chunk (comments
only). pre = pre-existing best-effort-prose sites left as-is. (b) = surfaced /
rewritten. c = flagged. fp = pattern false-positive (non-Result), reported.

| File                            | sites   | a-ann   | pre    | (b)    | c      | fp     |
| ------------------------------- | ------- | ------- | ------ | ------ | ------ | ------ |
| src/account.rs                  | 6       | 6       |        |        |        |        |
| src/activity.rs                 | 59      | 57      | 2      |        |        |        |
| src/ansi_cells.rs               | 4       | 4       |        |        |        |        |
| src/bundle.rs                   | 7       | 7       |        |        |        |        |
| src/calendar/recur.rs           | 1       |         |        | 1      |        |        |
| src/ci.rs                       | 2       | 2       |        |        |        |        |
| src/config.rs                   | 20      | 1       |        |        | 19     |        |
| src/config_resolve.rs           | 6       | 3       |        |        | 2      | 1      |
| src/config_tests.rs             | 22      | 22      |        |        |        |        |
| src/config_tests_coverage.rs    | 7       | 7       |        |        |        |        |
| src/config_write.rs             | 19      | 19      |        |        |        |        |
| src/connectivity.rs             | 8       | 8       |        |        |        |        |
| src/db.rs                       | 14      | 10      | 4      |        |        |        |
| src/db_aux.rs                   | 1       | 1       |        |        |        |        |
| src/db_cache.rs                 | 10      | 10      |        |        |        |        |
| src/db_calendar.rs              | 3       | 3       |        |        |        |        |
| src/db_hibernate.rs             | 1       |         |        | 1      |        |        |
| src/db_iroh.rs                  | 2       | 1       |        | 1      |        |        |
| src/db_migrate.rs               | 55      | 55      |        |        |        |        |
| src/db_model_proxy.rs           | 2       | 2       |        |        |        |        |
| src/db_pool.rs                  | 4       |         |        | 4      |        |        |
| src/db_projects.rs              | 2       | 2       |        |        |        |        |
| src/db_tests.rs                 | 29      | 29      |        |        |        |        |
| src/db_trust.rs                 | 2       | 2       |        |        |        |        |
| src/db_usage.rs                 | 1       | 1       |        |        |        |        |
| src/db_workspace.rs             | 3       |         | 1      | 2      |        |        |
| src/db_zones.rs                 | 2       | 2       |        |        |        |        |
| src/devcontainer.rs             | 2       | 2       |        |        |        |        |
| src/devcontainer_overlay.rs     | 1       |         |        |        |        | 1      |
| src/devenv.rs                   | 4       | 4       |        |        |        |        |
| src/diagnostics.rs              | 9       | 9       |        |        |        |        |
| src/diff_highlight.rs           | 2       |         |        |        |        | 2      |
| src/direnv.rs                   | 9       | 8       | 1      |        |        |        |
| src/disk.rs                     | 6       | 6       |        |        |        |        |
| src/dns_filter.rs               | 6       | 5       |        | 1      |        |        |
| src/envplan.rs                  | 15      | 15      |        |        |        |        |
| src/event_bus.rs                | 3       | 3       |        |        |        |        |
| src/file_manager/yazi.rs        | 27      | 16      | 11     |        |        |        |
| src/forge/mod.rs                | 24      |         |        | 24     |        |        |
| src/fsperm.rs                   | 4       | 3       |        |        |        | 1      |
| src/heal.rs                     | 1       |         | 1      |        |        |        |
| src/host_db.rs                  | 3       |         | 3      |        |        |        |
| src/i18n.rs                     | 1       | 1       |        |        |        |        |
| src/image.rs                    | 1       | 1       |        |        |        |        |
| src/jj.rs                       | 4       | 4       |        |        |        |        |
| src/layout_import.rs            | 2       | 2       |        |        |        |        |
| src/log_trace.rs                | 18      | 16      |        |        |        | 2      |
| src/lsp_registry.rs             | 1       | 1       |        |        |        |        |
| src/managed_tool.rs             | 1       | 1       |        |        |        |        |
| src/merge_guard.rs              | 9       | 9       |        |        |        |        |
| src/migrate_brand.rs            | 4       | 4       |        |        |        |        |
| src/models.rs                   | 1       | 1       |        |        |        |        |
| src/notification_route.rs       | 1       | 1       |        |        |        |        |
| src/out.rs                      | 3       | 3       |        |        |        |        |
| src/picker.rs                   | 7       | 7       |        |        |        |        |
| src/placement.rs                | 1       |         | 1      |        |        |        |
| src/plugin_api.rs               | 1       | 1       |        |        |        |        |
| src/profile.rs                  | 13      | 11      | 2      |        |        |        |
| src/remote.rs                   | 4       | 1       | 3      |        |        |        |
| src/remote_tune.rs              | 1       |         | 1      |        |        |        |
| src/repo.rs                     | 4       | 4       |        |        |        |        |
| src/repo_map.rs                 | 1       | 1       |        |        |        |        |
| src/retry.rs                    | 1       |         | 1      |        |        |        |
| src/sandbox.rs                  | 12      | 10      | 1      |        |        | 1      |
| src/sandbox_mounts.rs           | 2       | 2       |        |        |        |        |
| src/sandbox_prefetch.rs         | 6       | 6       |        |        |        |        |
| src/sandbox_preflight.rs        | 2       | 2       |        |        |        |        |
| src/sandbox_tests.rs            | 7       | 5       | 2      |        |        |        |
| src/semantic.rs                 | 2       | 2       |        |        |        |        |
| src/ssh_creds.rs                | 2       | 1       |        |        | 1      |        |
| src/startup.rs                  | 15      | 15      |        |        |        |        |
| src/termcaps.rs                 | 4       | 2       |        |        |        | 2      |
| src/util.rs                     | 18      | 17      | 1      |        |        |        |
| src/worktree.rs                 | 11      | 11      |        |        |        |        |
| src/zone.rs                     | 2       | 2       |        |        |        |        |
| tests/devcontainer_e2e.rs       | 8       | 7       |        |        |        | 1      |
| tests/merge_guard_hook.rs       | 10      | 10      |        |        |        |        |
| tests/repo_issues_overlay.rs    | 3       | 3       |        |        |        |        |
| tests/sandbox_credentials.rs    | 1       | 1       |        |        |        |        |
| tests/sandbox_dns_e2e.rs        | 3       | 3       |        |        |        |        |
| tests/sandbox_health.rs         | 2       | 2       |        |        |        |        |
| tests/sandbox_lifecycle.rs      | 1       | 1       |        |        |        |        |
| tests/sandbox_network_policy.rs | 4       | 4       |        |        |        |        |
| tests/sandbox_profile.rs        | 1       | 1       |        |        |        |        |
| **total**                       | **603** | **501** | **35** | **34** | **22** | **11** |

## Unsure — for the reviewer

1. **`db_migrate.rs` (all 55 annotated (a)) — class note.** The ignore is the
   documented idempotent no-op ("already applied"), but nothing discriminates
   error kinds: an `ALTER TABLE … ADD COLUMN` failing for a _different_
   reason (locked / full DB) would silently skip an upgrade step while the
   schema version still advances. A `has_column`-guarded rewrite (the pattern
   the v50+ migrations already use) would make the ignore purely
   belt-and-braces. Left annotated, not rewritten — 55 sites of behaviour
   change exceeds this chunk's containment budget.
2. **`config.rs:5725-5928` (19 sites, (c))** — env-override enum fallbacks
   (`THEGN_PICKER=bogus` silently keeps the default). The design's A5
   condition was "annotate iff `config validate` already reports the invalid
   value" — no such reporting exists (`config_warn` is used for floats and
   `--set`, but not for the enum env knobs). Either add `config_warn` per site
   (B1-style) or document the silent fallback as the contract.
3. **`config_resolve.rs:1403` (c)** — a malformed _profile overlay_ is
   silently unapplied in the resolve-stages report (`let Ok(ps)` guard +
   ignored apply). Should the stages report carry an event, like the
   ClampEvents?
4. **`config_resolve.rs:1412` (c)** — a bad `--set k=v` in the stages resolver
   is silently dropped, while the real startup path (`config.rs:6024`) warns
   via `config_warn`. Consider recording an event for parity.
5. **`ssh_creds.rs:56` (c)** — `set_permissions(0o600)` on the flattened
   ssh_config is ignored. The parent dir is created with default perms
   (`create_dir_all`, 0755-ish), so a failed tighten can leave the file
   world-readable inside a world-traversable dir. Content is config (no
   keys), but a warn or a 0700 dir creation would be cheap insurance.
6. **`db_cache.rs` (10 sites, (a))** — annotated per the module doc ("pure
   caches — best-effort, git/live-API is the source of truth"). If the panel
   treats a failed read identically to an empty cache without refetching,
   these deserve the same warn-match treatment db_pool got. I did not trace
   every caller to confirm refetch-on-miss.

## Pattern false-positives (non-Result discards, left pinned)

`config_resolve.rs:907` (`let _ = r;` unused match-arm binding),
`devcontainer_overlay.rs:141` (bool discard, side-effecting `ok()`),
`diff_highlight.rs:66-67` (OnceLock warming, `&'static` returns),
`fsperm.rs:72` (cfg'd-out platform stub), `log_trace.rs:216` (`run_id()`
warming, returns `&'static str`), `log_trace.rs:904` (deliberate out-of-bounds
panic exercising the crash reporter), `sandbox.rs:1584` (`let _ = expr?;`
— error already propagated), `termcaps.rs:1797/1806` (kani harness nondet
consumption), `tests/devcontainer_e2e.rs:247` (`let _ = container_name;`
silence-unused). Per the design these only leave the list via
`drop()`/renames when that releases the file — none do.

## Gate / test verification (scoped, per dev-loop policy)

```
$ just quick thegn-core
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 4m 37s   (clean, no warnings)

$ cargo nextest run -p thegn-core
     Summary [ 20.006s] 3509 tests run: 3509 passed, 2 skipped

$ bash test/ratchet.sh ignored-result 'let _ = |let _ =[[:space:]]*$|\.ok\(\);' crates
ratchet(ignored-result): clean (321 pinned)
```

Note on the scoped ratchet run the chunk spec asked for
(`… crates/thegn-core`): ratchet.sh compares the pathspec's hit set against
the _whole_ allowlist, so a narrow pathspec reports every other crate's
pinned file as "stale" — noise, not signal. The authoritative check is the
full-`crates/` run above (this is also what `just lint` runs): it is clean,
which proves (i) no new violations anywhere, (ii) every still-pinned file
still matches, and (iii) the four released `thegn-core` files are off the
list. A `thegn-core`-filtered run of the full output contains zero errors.

## Unverified

- **clippy/compile of the crate's test targets is exercised by nextest (they
  compiled and ran), but `just quick` only lints lib/bin** — pre-push
  (`just test`) runs the full clippy pass including `#[cfg(test)]` code and
  `tests/`; my test-file edits are comments only, so exposure is minimal.
- **`just coverage` (95% thegn-core gate) was not run** — CI-only by policy.
  The (b) rewrites add error branches that unit tests don't hit (they'd need
  a fault-injected DB). If the coverage gate measures the new warn branches,
  those lines are not in the `cov_ignore` exclusion set, so coverage could
  shift by a few lines. Flagged for the fold.
- **Windows/other-platform compile** — touched code is either comments or
  platform-neutral; the one cfg'd-out stub (`fsperm.rs:72`) was left
  untouched. Not compiled on other targets here.
- Final formatting is verified by the pre-commit treefmt hook at commit time.
