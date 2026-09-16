# Maintenance08 THE321 redirect matrix independent review — 2026-09-15

**Candidate:** `759cd1ad252cb131c87cf9078923c31203cdfeb5`, source file `crates/thegn-svc/src/issue/http.rs` (SHA-256 `927a24b747eecc6a96a4cd696e720a2694824488dd11fddf3542e0ae06f2670d`).

This is a test-only expansion; no production code changed. It matches the literal THE321 acceptance matrix: 301, 302, 307, and 308 × same-origin, cross-origin, and loop destinations × GET and POST, for 24 cases. Every case requires the typed redirect refusal, exactly one authenticated source request, and zero `/target` hits across both the source and separate cross-origin target listener. POST carries a mutation replay sentinel, so mutation requests are included. Both listener tasks are aborted and awaited.

The production client uses `reqwest::redirect::Policy::none`, so the response is classified before automatic follow-up. The fixture’s source hit counter plus shared target counter would catch same-origin, cross-origin, or loop replay. No literal redirect acceptance gap remains in source.

The corrected 104/104 receipt at `/tmp/thegn-maintenance08-focused-native-fixture-receipt-20260915.json` is source `f46e043d649a639759333d76faeb6abb6749f2a5` and predates this commit. It does not evidence the expanded fixture; a fresh narrowed rerun is required.

**Verdict: approved for the narrow rerun; final evidence pending that rerun.** No builds, tests, native/provider calls, or edits were performed.
