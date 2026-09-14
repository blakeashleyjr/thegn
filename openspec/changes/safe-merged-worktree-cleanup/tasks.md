# THE-588

- [x] Implement verified repository/checkout identity and target ancestry checks.
- [x] Preserve dirty, ignored, unknown and refused worktrees without force.
- [x] Add typed removal/report/bookkeeping outcomes and worker identity claims.
- [x] Add bounded Git ownership, fallible-worker and ref-transaction regressions.
- [x] Add private shared-database and cached-foreign-repository regressions.
- [x] THE-593: require exact landed selection and committed-outcome identity; compare-and-delete manual row clearing.
- [x] THE-593 September 14 follow-up: execute real-commit result-only mutation and stale manual-clear regressions; independently review explicit same-value ABA proof limit.
- [x] THE-594: settle local teardown eligibility and refuse unmanaged runtime ownership, including historical OCI uncertainty.
- [x] Remove unproven automatic ref mutation; preserve a full-row-CAS branch hold through cache reaping.
- [ ] THE-596 follow-up: prove atomic direct-ref-type admission before restoring automatic branch deletion.
- [x] Run focused private tests (16 core, 78 host) and source/architecture ratchets.
- [x] THE-600: verify strict registry/resource observations reject malformed remote, tenancy and dispatch rows before hooks and after admission (18 core, 81 host focused tests passed).
- [x] THE-600 September 14 follow-up: execute the real post-hook registry-decoder corruption regression and independently review it before closure.
- [x] Complete primary and independent review of THE-593/THE-594/THE-600 scoped repairs; broader THE-588/THE-596 obligations remain separate.
- [x] Integrate the reviewed scoped repairs with THE-589 and pass the private native delivery gate; live queue cleanup is separate.

The reviewed scoped changes are delivered to local main. No live cleanup or runtime mutation is claimed.

## THE-588 actual successful-land acceptance

- [x] Identify the missing positive proof: prior cleanup fixtures seeded landed metadata without performing a new target advance.
- [x] Obtain primary and independent approval of a single-environment-lock, two-repository actual fold/persist/cleanup fixture.
- [x] Execute the real target CAS followed by production persistence and configured removal; verify foreign state and retained branch/queue hold.
- [x] Pass all 38 selected native lifecycle/cleanup and failed-fold tests at `b21cb20d`.
- [x] Pass strict host lint and source gates; land the reviewed fixture on local main before closing THE-588 (`bb8ff6d7`).

## THE-594 actual resource-admission regression follow-up

- [x] Obtain primary approval for private registry custody and a path-scoped explicit-teardown entry observer, all cfg(test).
- [x] Add actual automatic-cleanup cases for projection, provider sync and both; retain exact queue/cache/refs/content and refuse before hooks.
- [x] Add same-selection positive cleanup after registry release, custody unwind restoration, and worktree-selection change alongside workspace-selection coverage.
- [x] Run focused actual host tests (cleanup50/50 including both new runtime fixtures) and independent source review; exact results recorded in the dated THE-594 audit.
