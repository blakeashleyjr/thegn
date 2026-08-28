# THE-81 chunk 1 — Audit every ignored Result in `thegn-core`

**Read `.thegn/pipeline/THE-81/architect/design.md` first** — the classification
rubric, the handling idioms, and the containment rules there are binding for
this chunk. This file is the executable subset for the core crate.

Goal: for every site in the pinned files below that matches
`let _ = `, `let _ =$` (rustfmt-wrapped), or `.ok();`, either (a) annotate the
sanctioned best-effort ones `// best-effort: <why>`, (b) surface the
primary-path ones (then delete the file's line from the allowlist), or (c)
leave untouched and list it for the reviewer. Comments-only for (a); handling
for (b); nothing speculative for (c).

## Files touched (exact)

Code scope — every pinned file under `crates/thegn-core/` (84 files, 603 sites
at base `d361b60a`; the list below is the authority, taken from
`test/ignored-result-ratchet.txt`):

```
crates/thegn-core/src/account.rs
crates/thegn-core/src/activity.rs
crates/thegn-core/src/ansi_cells.rs
crates/thegn-core/src/bundle.rs
crates/thegn-core/src/calendar/recur.rs
crates/thegn-core/src/ci.rs
crates/thegn-core/src/config_resolve.rs
crates/thegn-core/src/config.rs
crates/thegn-core/src/config_tests_coverage.rs
crates/thegn-core/src/config_tests.rs
crates/thegn-core/src/config_write.rs
crates/thegn-core/src/connectivity.rs
crates/thegn-core/src/db_aux.rs
crates/thegn-core/src/db_cache.rs
crates/thegn-core/src/db_calendar.rs
crates/thegn-core/src/db_hibernate.rs
crates/thegn-core/src/db_iroh.rs
crates/thegn-core/src/db_migrate.rs
crates/thegn-core/src/db_model_proxy.rs
crates/thegn-core/src/db_pool.rs
crates/thegn-core/src/db_projects.rs
crates/thegn-core/src/db.rs
crates/thegn-core/src/db_tests.rs
crates/thegn-core/src/db_trust.rs
crates/thegn-core/src/db_usage.rs
crates/thegn-core/src/db_workspace.rs
crates/thegn-core/src/db_zones.rs
crates/thegn-core/src/devcontainer_overlay.rs
crates/thegn-core/src/devcontainer.rs
crates/thegn-core/src/devenv.rs
crates/thegn-core/src/diagnostics.rs
crates/thegn-core/src/diff_highlight.rs
crates/thegn-core/src/direnv.rs
crates/thegn-core/src/disk.rs
crates/thegn-core/src/dns_filter.rs
crates/thegn-core/src/envplan.rs
crates/thegn-core/src/event_bus.rs
crates/thegn-core/src/file_manager/yazi.rs
crates/thegn-core/src/forge/mod.rs
crates/thegn-core/src/fsperm.rs
crates/thegn-core/src/heal.rs
crates/thegn-core/src/host_db.rs
crates/thegn-core/src/i18n.rs
crates/thegn-core/src/image.rs
crates/thegn-core/src/jj.rs
crates/thegn-core/src/layout_import.rs
crates/thegn-core/src/log_trace.rs
crates/thegn-core/src/lsp_registry.rs
crates/thegn-core/src/managed_tool.rs
crates/thegn-core/src/merge_guard.rs
crates/thegn-core/src/migrate_brand.rs
crates/thegn-core/src/models.rs
crates/thegn-core/src/notification_route.rs
crates/thegn-core/src/out.rs
crates/thegn-core/src/picker.rs
crates/thegn-core/src/placement.rs
crates/thegn-core/src/plugin_api.rs
crates/thegn-core/src/profile.rs
crates/thegn-core/src/remote.rs
crates/thegn-core/src/remote_tune.rs
crates/thegn-core/src/repo_map.rs
crates/thegn-core/src/repo.rs
crates/thegn-core/src/retry.rs
crates/thegn-core/src/sandbox_mounts.rs
crates/thegn-core/src/sandbox_prefetch.rs
crates/thegn-core/src/sandbox_preflight.rs
crates/thegn-core/src/sandbox.rs
crates/thegn-core/src/sandbox_tests.rs
crates/thegn-core/src/semantic.rs
crates/thegn-core/src/ssh_creds.rs
crates/thegn-core/src/startup.rs
crates/thegn-core/src/termcaps.rs
crates/thegn-core/src/util.rs
crates/thegn-core/src/worktree.rs
crates/thegn-core/src/zone.rs
crates/thegn-core/tests/devcontainer_e2e.rs
crates/thegn-core/tests/merge_guard_hook.rs
crates/thegn-core/tests/repo_issues_overlay.rs
crates/thegn-core/tests/sandbox_credentials.rs
crates/thegn-core/tests/sandbox_dns_e2e.rs
crates/thegn-core/tests/sandbox_health.rs
crates/thegn-core/tests/sandbox_lifecycle.rs
crates/thegn-core/tests/sandbox_network_policy.rs
crates/thegn-core/tests/sandbox_profile.rs
```

Plus exactly two audit files:

- `test/ignored-result-ratchet.txt` — **delete the lines of files you
  released** (a file is releasable only when zero non-comment lines in it
  match the pattern any more). Deletions only; never add a line; never run
  `RATCHET_UPDATE=1` on this ratchet; never touch the `#` header (chunk 3
  owns the header prose).
- `.thegn/pipeline/THE-81/code/chunk-1-done.md` — the report (format below).

## Overlap / dependency

**None — fully file-disjoint from chunks 2 and 3; the Lead may run all three
in parallel.** The shared allowlist is safe: you delete only
`^crates/thegn-core/` lines; chunks 2/3 delete only their own crates' lines
(disjoint ranges of one file, merges cleanly). Do not edit `justfile`,
`test/ratchet.sh`, or any file outside the two scopes above. If run serially,
this chunk goes **first** (core is upstream of host and svc).

## Approach (per-site loop)

For each file in the scope list: `git grep -nE 'let _ = |let _ =[[:space:]]*$|\.ok\(\);' -- <file>`
(this includes comment-only lines; ignore those), read each site's context,
classify A1-A5 / B1-B3 / C per the design's rubric, apply the action. Site
shapes already confirmed in this crate (evidence for the table below):
`util.rs:263` (OnceLock set), `:726` (unlock), `:818` (stdout flush at exit),
`:1004-1234` (tmp `remove_dir_all` — cleanup); `heal.rs:29` (already
annotated); `activity.rs:897` (already annotated); `db_migrate.rs:199-229`
(`ALTER TABLE … ADD COLUMN` probes); `account.rs:511,553,609`
(`remove_dir_all` cleanup); `envplan.rs:149` / `account.rs:179-180`
(optional-input `.ok()`); `config.rs:5725-5793` (enum fallbacks).

| Hot file (sites)                                                                                                                 | Dominant shapes                                                            | Provisional class                                                                   | Watch for                                                                                                      |
| -------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------- | ----------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------- |
| `activity.rs` (59)                                                                                                               | tmp-file drops, child reaping (already annotated at :897, :932)            | (a)                                                                                 | any non-cleanup swallow in the poll path                                                                       |
| `db_migrate.rs` (55)                                                                                                             | `let _ = conn.execute("ALTER TABLE … ADD COLUMN …", [])` probes (:199-229) | (a) _if_ the ignore is the duplicate-column no-op — annotate why                    | an ALTER failing for a _different_ reason (locked/full DB) silently no-ops the migration → that's (c), flag it |
| `file_manager/yazi.rs` (27)                                                                                                      | already carries `// best-effort (see prepare)` prose (:168-379)            | (a) — existing prose satisfies the intent; do not mass-rewrite                      | —                                                                                                              |
| `forge/mod.rs` (24)                                                                                                              | cache writes + response parse fallbacks                                    | (a)/(c) per rubric                                                                  | a parse fallback that hides an auth/token error is (b)                                                         |
| `config.rs` (20)                                                                                                                 | `from_str_validated(v).ok()` enum fallbacks (:5725-5793)                   | (a) **iff** `thegn config validate` reports the same invalid value; else (b) or (c) | verify the validate path actually surfaces it before annotating                                                |
| `config_write.rs` (19)                                                                                                           | rollback restores after a failed write                                     | (a) — the primary error is reported by the caller (cf. `cmd/config.rs:89-97`)       | —                                                                                                              |
| `util.rs` (18)                                                                                                                   | OnceLock `set` (first-set-wins), unlock, stdout flush, tmp cleanup         | (a)                                                                                 | `util.rs:803` shell spawn — check what failure means before classifying                                        |
| `log_trace.rs` (18)                                                                                                              | sink install / crash-report paths, partially annotated already             | (a)                                                                                 | —                                                                                                              |
| `startup.rs` (15), `envplan.rs` (15), `db.rs` (14), `profile.rs` (13), `sandbox.rs` (12), `worktree.rs` (11), `db_cache.rs` (10) | cache writes, optional-input reads, cleanup                                | (a) mostly                                                                          | DB reads whose `None` silently degrades a _reported_ surface → (b)                                             |
| everything else ≤10 sites                                                                                                        | mixed                                                                      | classify per rubric                                                                 | —                                                                                                              |
| `tests/*.rs` (9 files)                                                                                                           | scratch teardown, socket/dir cleanup                                       | (a) annotate                                                                        | a `let _ =` hiding a _setup_ failure that voids the assertion is (c)                                           |

Release rule reminder: **an annotation never releases a file** — only sites
handled so the pattern no longer matches do (`match`/`if let Ok`, `?`, or
`drop(...)` for non-Result discards; **not** `.inspect_err(…).ok()`). After
handling a file's last matching site, delete its line from the allowlist.

## Tests to run (scoped; no full-workspace builds)

1. `just quick thegn-core` — clippy on lib/bin must stay clean.
2. `cargo nextest run -p thegn-core` — full crate suite (the sweep spans the
   crate; comments must not break `db_tests` / `config_tests` / `sandbox_tests`).
3. `bash test/ratchet.sh ignored-result 'let _ = |let _ =[[:space:]]*$|\.ok\(\);' crates/thegn-core`
   — must print `clean` with your deletions applied (proves no stale entry and
   no new violation in this crate).

## Done-criteria

1. Every in-scope site is annotated (a), handled (b), or listed (c) — spot
   check: `git grep -nE 'let _ = |let _ =[[:space:]]*$|\.ok\(\);' -- crates/thegn-core | grep -v 'best-effort'`
   returns only (b)-rewritten sites, (c) sites you list, and comment-only lines.
2. All three test commands above green.
3. `test/ignored-result-ratchet.txt` contains strictly fewer `^crates/thegn-core/`
   lines than at base (deleted exactly the released files), nothing added.
4. `chunk-1-done.md` written with: a per-file table (file → sites → a/b/c
   counts → action taken), the **Unsure — for the reviewer** list (file:line +
   the question), and the ratchet command output snippet.
5. Commit the chunk with subject exactly:

   `fix(the-81): audit thegn-core ignored-results — annotate best-effort, surface primary-path errors`
