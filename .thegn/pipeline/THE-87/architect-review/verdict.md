# THE-87 — Architecture review verdict: APPROVED

Reviewer: ARCHITECT
Branch: `tg/the-87-live-fallback-workspace` (implementation reviewed at `9a5af10a`; verdict commit follows)
Base: `main` (merged first; already up to date)

Verdict: **APPROVED**

## Scope and findings

Reviewed the complete `git diff main...HEAD`, the design, all code chunks and
done-docs, the revision chunk, `CLAUDE.md`, `docs/ARCHITECTURE.md`, the OpenSpec
sidebar/state contracts, and the relevant ratchets.

- The core schema gate now uses `on_disk >= current`, preserves
  `schema_mismatch`, and emits the newer-schema warning once per process.
- The two sidebar DB-read swallows now log errors; the pure registry heal is
  applied at hydration and switch-refresh model boundaries.
- Sidebar worktree creation refuses an unresolved workspace row and preserves
  active-tab fallback only when no sidebar row is in play. The global action,
  composite action, sidebar key, context menu, and template action all use the
  shared target logic. The widened duplicate-lookup grep is clean.
- No new ignored-result debt, action/help-ratchet debt, blocking loop I/O, or
  out-of-scope source changes were found. `git diff --check` is clean.

No correction or revision chunk is required. The previously recorded
third-site revision is implemented in `ad5b5e49` and included in this review.

## Verification

- `git merge main`: already up to date; `main` is an ancestor of `HEAD`.
- Mandatory host gate: **355/355 passed**:
  `cargo nextest run -p thegn-host -E 'test(complete) | test(help) | test(catalog_tests) | test(platform_ratchet) | test(sidebar)'`.
- THE-87 core regressions: **4/4 passed** (`open_mode`, `newer_db`,
  `fast_reopen`, `detect_newer_schema`).
- Full mandatory core filter: **493/495 passed**; the two failures are the
  unchanged `sandbox::tests::oci_local_secrets_go_to_env_file_not_argv` and
  `sandbox::tests::systemd_local_secrets_go_to_environment_file_not_argv`.
  No THE-87 file touches that subsystem.
- `just quick thegn-core` and `just quick thegn-host`: passed.
- Workspace `cargo fmt --all -- --check` reports only the pre-existing,
  unrelated formatting difference in `crates/gtui-app/src/engine.rs`.

## Unverified items

The chunk done-docs identify no full-workspace `just test`/`just ci` or e2e run;
this is acceptable under the repository's dev-loop policy and does not block
this scoped architecture approval. The optional diagnostics-ring assertion
was omitted; the once-per-process guarantee is review-verifiable from the
single `std::sync::Once` guard in `db.rs`.

No revision chunk paths apply.
