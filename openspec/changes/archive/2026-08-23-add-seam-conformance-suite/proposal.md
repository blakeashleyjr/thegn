## Why

Each seam's tests pin its own behavior (kind coverage, ladder order), but nothing asserted the _cross-seam_ contract: that every probe report names a known seam and a non-empty id, that `Unavailable` always carries a reason, that reserved selections say so, that per-account factories build exactly the implemented kinds, and that probes are cheap, deterministic snapshots. A new seam or provider could quietly ship a malformed probe and doctor output would degrade without any gate noticing.

## What Changes

- New `thegn_svc::conformance`: `KNOWN_SEAMS`, `assert_report_invariants`, `seams_of` — reusable shape assertions over `seam::registry::probes` output.
- Conformance tests: default-config shape; fully-configured registry (one report per issue/calendar account); reserved selections report "reserved" (ci=drone, media=jellyfin); missing binary reported by name (`binary_availability`); per-account factory coverage (`backend_from_account` for issues and calendar returns `Some` exactly for implemented, non-`none` kinds — the account analogue of `seam::kind_coverage`); probe determinism.
- `issue::backend_from_account` / `calendar::backend_from_account` widened to `pub(crate)` for the coverage test.

## Impact

- Audit row B3 (conformance suite). Doctor's Providers section was already smoke-asserted (`test/smoke.sh`); per-kind coverage already existed for ci/git/forge — this closes the account-based seams and the registry shape.
- Specs: provider-seams delta (registry conformance requirement).
- Code: thegn-svc only.
