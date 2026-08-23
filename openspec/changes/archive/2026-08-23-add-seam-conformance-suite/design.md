## Context

Per-seam kind-coverage tests exist (ci/git/forge via `seam::kind_coverage`; sandbox's profile table in core), and smoke asserts doctor's Providers section. Missing: shape invariants over the whole registry and coverage for the account-selected seams.

## Goals / Non-Goals

- Goals: one reusable assertion set over `registry::probes` output; account-factory coverage; keep probes honest (cheap, deterministic, reasons on failure).
- Non-Goals: network probing (probes are offline by contract); per-provider behavioral tests (those live with each seam).

## Decisions

- **Library helpers + in-crate tests**, not a dev-only crate: `conformance` is a normal svc module so future host tests can reuse `assert_report_invariants`; the tests live beside it.
- **Account factories become `pub(crate)`**, not public: the crate's own tests are the only consumer; routers stay the public door.
- **Determinism is asserted on (seam, id) sequence**, not full reports — notes may legitimately embed resolved paths that differ across environments.
