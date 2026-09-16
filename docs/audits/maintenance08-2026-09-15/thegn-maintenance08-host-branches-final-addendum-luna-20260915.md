# Maintenance08 host branch final addendum — 2026-09-15

Candidate `f46e043d649a639759333d76faeb6abb6749f2a5`. This is an independent source assessment of the two host branches previously marked as selection gaps.

`issue_backends` validates a plugin namespace in `PluginIssueBackend::new` before leaking the provider id. `extend_issue_router` then passes only those concrete backends to `IssueRouter::push_backend`, which repeats the same namespace validation. Therefore the `push_backend` error arm is currently unreachable for this construction path unless the shared contract diverges. It is safe defense in depth: an invalid registry row is dropped and a registration error is logged without insertion. The service tests cover the underlying constructor and router contracts, but the host wrapper’s invalid-row/conflict branches remain unexecuted. This is a documented coverage gap, not a correctness blocker.

`spawn_issue_cache_refresh` decodes the prior per-account cache, treats malformed JSON as empty, and filters invalid `Issue` identities before diff notifications. Provider failure returns earlier and preserves the old cache, so invalid stale rows do not produce refresh notifications through this path. The panel hydration fixture covers the separate `populate_tracker` filter and the diff fixture covers event semantics; neither drives the refresh worker with malformed old data. This remains an untested branch. The filter does not bind an otherwise valid `Issue` back to the DB provider-key column; that is outside this change and is a possible future cache-integrity seam, not a demonstrated unsafe behavior.

These findings support the root’s scoped disposition: no host rebuild is justified solely by the two absent wrapper selectors, provided the final evidence explicitly qualifies them and does not claim exhaustive host coverage. The corrected f46 service receipt is still required; the original 4256 receipt remains historical at 106 pass / 1 fail.

The reusable THE327 pointer is `thegn_core::fs_custody::LosslessPath::from_path` from `b191cce5c6fd00a305132748d51bd3a5d81b0735`; platform path conversion stays in `fs_custody`.
