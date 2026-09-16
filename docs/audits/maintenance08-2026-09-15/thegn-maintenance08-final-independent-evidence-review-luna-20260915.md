# Maintenance08 THE321/THE322 final independent evidence review — 2026-09-15

**Candidate:** `759cd1ad252cb131c87cf9078923c31203cdfeb5`.

**Verdict: approved scoped final evidence.**

The final evidence accounts for 107 distinct tests: 103 unchanged service tests from the corrected f46 receipt, one changed redirect selector from the 759 supplement, and three unchanged host tests from the preserved 4256 receipt. The redirect selector replaces the older 302-only version, so the accounting is `103 + 1 + 3 = 107`.

The redirect supplement at `/tmp/thegn-maintenance08-focused-native-redirect-receipt-20260915.json` is source 759, exact 1/1 pass, and its fixture executes 24 cases: 301/302/307/308 × same/cross/loop × GET/POST. Every case requires typed refusal, one authenticated source request, zero target/follow-up requests, and joined fixture servers.

The corrected f46 receipt remains 104/104 pass. The earlier 4256 receipt remains historical at 106 pass / 1 fixture fail. Neither receipt was rewritten.

Scoped Clippy for source 759 exits 0. Native artifact admission exits 0 and records the exact binary and depfile hashes. `compiler_artifact_fresh: false` is recorded as cache status only: the build checkout is at 759 and its `issue/http.rs` hash matches the review checkout exactly (`927a24b7…f2670d`). Production-prefix equivalence validates production against 4256 and reports only test-region changes in `issue/http.rs` and `issue/mod.rs`.

The host plugin-wrapper and refresh-worker selectors remain explicitly untested, as previously documented. Source review found no material unsafe behavior in those branches, so they do not block this scoped THE321/THE322 acceptance. No claim is made for unrelated host branches or hostile same-UID atomic custody.

No builds, tests, native/provider calls, database operations, or source edits were performed by this review.
