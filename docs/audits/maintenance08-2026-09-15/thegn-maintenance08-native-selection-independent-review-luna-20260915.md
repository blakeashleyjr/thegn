# Maintenance08 native selection independent review — 2026-09-15

**Candidate:** `f46e043d649a639759333d76faeb6abb6749f2a5` in `/tmp/thegn-maintenance08-combined-20260915`.

The `f46e043d` correction is approved for the next rerun. It adds `"updated_at_ms": 0` to the `cache_boundary_rejects_cross_provider_and_malformed_ids` JSON fixture in `crates/thegn-svc/src/issue/mod.rs`. `Issue.updated_at_ms` is a required `i64` serde field, so the prior 4256 receipt failed while constructing the fixture and never reached the identity assertions. This is test data only; production code is unchanged.

The original receipt at `/tmp/thegn-maintenance08-focused-native-receipt-20260915.json` is historical: source `4256d2372057a02a1f6fea96cdc4ce8388911178`, 106 pass and 1 fail, with the sole failure `issue::spec::cache_boundary_rejects_cross_provider_and_malformed_ids`. It must remain labeled historical until a new receipt is produced from f46.

The corrected selection is strong for the service scope. Its prefixes cover all selected HTTP, identity, GitHub/Jira/Kaneo/Linear, capability, plugin bridge, router-spec, and Linear schema tests. The real control-router fixtures cover opaque identity round trips and malformed GET/update/comment requests with zero fake provider calls. HTTP fixtures cover queued serialization, permit release, redirect credential refusal, content encoding, MIME/status, chunked limits, and multi-request budgets. Jira, Kaneo, and Linear each have provider sequence budget fixtures.

Two host coverage gaps remain if the receipt claims all changed provider/cache paths:

1. `crates/thegn-host/src/plugin_providers.rs:32-58` now drops invalid `PluginIssueBackend::new` rows and rejects `IssueRouter::push_backend` failures. The selected `plugin::provider::tests` are service tests; the only host registry fixture uses a valid namespace. Add a host fixture for invalid namespace and registration conflict, asserting refusal without panic or dispatch.
2. `crates/thegn-host/src/hydrate_tracker.rs:99-107` filters malformed old cache entries before refresh diffing. The selected panel hydration fixture exercises `populate_tracker`, and the diff fixture calls the pure diff function; neither runs `spawn_issue_cache_refresh` with malformed old-cache data. Add an owned refresh fixture asserting malformed rows cannot emit automation events while valid rows remain eligible, or record this branch as untested.

The reviewed THE327 custody pointer for startup is `/tmp/thegn-THE327-impl-luna-20260915-v1` at `b191cce5c6fd00a305132748d51bd3a5d81b0735`. Reuse `thegn_core::fs_custody::LosslessPath::from_path` for lossless path capture and keep platform conversion in `fs_custody`; do not duplicate `OsStr` or UTF-16 handling in business consumers. `LosslessPath::from_path` is public in this slice; `validate`, `append_wire`, and `secure_random` are crate-private and should not be bypassed by cross-crate reimplementations.

No tests, builds, native/provider calls, database operations, or mutable target operations were run.
