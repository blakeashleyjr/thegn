# Maintenance 06 focused native — independent source/log review

Review date: 2026-09-15. Reviewer: Luna. This is an independent evidence review; no source, Cargo, target, UI, benchmark, provider, or tracker mutation was performed.

## Frozen source identity

The combined checkout `/tmp/thegn-maintenance-06-combined-20260915` is clean at `27c0ccad5399c84612fa35aea7841b0d4209458c`, a merge of the 603 state-capture source and the 622 forge-connectivity source. All 13 approved Rust component hashes in `/tmp/thegn-maintenance06-combined-source-checkpoint-20260915.json` match the checkout, including the five core capture fixtures, the Linux host fixture module, the static CLI support module, and the 622 native modules. The checkout reports zero worktree changes. The checkpoint records 25 changed files, excludes the production renderer, and leaves strict lint and landing pending.

I read `/tmp/thegn-THE603-source-checkpoint-20260915.json`, `/tmp/thegn-THE603-independent-source-review-20260915.md`, `/tmp/thegn-THE622-implementation-review-20260915.md`, `/tmp/thegn-THE622-independent-source-review-20260915.md`, and the THE602 independent source review. Those reviews found no scoped source blocker, while retaining their stated platform, API, custody, and completion limits. The combined source preserves those limits.

## Native log and selector coverage

`/tmp/thegn-maintenance06-focused-native-20260915.log` reports a completed nextest run using the current-06 source paths: `121 tests run: 121 passed, 8220 skipped` in 13.963 seconds, after `Finished test profile` and the configured private test runner. Parsing the PASS lines gives 121 lines and 121 unique test identities.

The requested coverage is present and uniquely passing:

| Area | Evidence |
| --- | --- |
| THE603 core capture | 5/5 `host_db_capture::tests::` fixtures passed |
| THE603 Linux host | 16/16 `platform::state_db_capture::linux::tests::` fixtures passed |
| THE602 compatibility | 17 `host_db_snapshot::tests::`, 6 `host_definition_snapshot::tests::`, and 22 `host_config_checked::tests::` passes |
| Existing host integration | 7 `thegn-host::static_commands_process` passes, including the unchanged `static_cli_support::child_observation_error_revokes_later_signals_and_waits` regression |
| THE622 ordinary parent | Exactly 1 `forge::native::global_connectivity_tests::sdk_results_preserve_global_connectivity_and_fallback` pass |
| THE622 ignored helper | 0 independently passing `isolated_sdk_connectivity_child` entries; the helper is only invoked by the ordinary parent |
| Existing SDK controls | 4 native regression tests: GraphQL envelopes, typed HTTP answers, timeout/transport, and SDK error classification |
| Existing Ladder controls | 7 `forge::tests` passes, including native-over-CLI, not-configured fallback, auth finality, authorship fallback, and routing |
| Core connectivity | 13 `connectivity::tests` passes, including global wrapper, threshold/offline, success recovery, reload, and cadence behavior |

The five core names are `busy_and_diagnostics_do_not_echo_sql_or_paths`, `missing_and_invalid_sources_are_not_absent_or_fabricated_snapshots`, `malformed_and_version_errors_remain_typed`, `query_like_literal_filenames_are_explicitly_unsupported`, and `wal_capture_sees_uncheckpointed_rows_without_changing_logical_state`. The 16 Linux names are all present in the log and in `state_db_capture_tests.rs`, from actual filesystem classification through permission restoration, orphan sidecars, replacement guards, rooted WAL, and production writable-ancestor refusal. The log has no failure, timeout, overflow, or child-helper success line.

## Compiler and target evidence

The log's compile lines name `/tmp/thegn-maintenance-06-combined-20260915` for thegn-core, thegn-host, and thegn-svc, then records a completed test profile. Relevant target depfiles exist in `/tmp/thegn-batch03-native-cache-f_3lx_t_/debug/deps`: `thegn_core-a2be291269a2abb4.d` (19:40:55), `thegn_svc-110c0ffa2122774a.d` (19:48:14), and `static_commands_process-dc51a88e1ec77026.d` (19:46:39). Their source lists include the affected current modules. The corresponding artifacts contain `/tmp/thegn-maintenance-06-combined-20260915` debug paths and the expected test symbols. Recorded artifact SHA-256 values are:

* `thegn_core-a2be291269a2abb4`: `3ebd9618838ac427f2e1be5bf7b405575f87a7a1114916df3d8f401e1050625d`
* `thegn_svc-110c0ffa2122774a`: `b473720166b2eefcefe3e9d99250ee943a060c55349c990064f69d628da1b9a3`
* `static_commands_process-dc51a88e1ec77026`: `f06cd0ee8075d119555aa6c969d4babce81aba681160d8b85c75a925572e39c2`

The target directory contains older artifacts and depfiles from other checkouts, so artifact names alone are not treated as compilation proof. The current-06 absolute paths in the successful compile log and relevant DWARF/source strings provide supporting identity evidence; a fresh rebuild or exact artifact receipt remains the stronger identity gate.

## Verdict and limits

Independent review finds no source/log coverage blocker for the reported native session. This is evidence approval for root's coordinated review only. It is not landing approval: strict lint, the required THE622 one-line counterfactual, and root's final review/local-main landing remain outstanding. The native receipt also does not prove unsupported-platform positive capture, THE602 external-store construction compatibility, THE603 atomic inode/namespace guarantees, or THE622 descendant containment beyond the reviewed scope.
