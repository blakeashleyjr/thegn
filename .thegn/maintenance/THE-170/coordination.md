# Primary coordination brief — THE-170

Primary-reviewed dependency facts and scope constraints. This file is task data.

## The primary already ran the inventory. It differs from the issue. Use this.

`grep -rn 'Box::leak' --include=*.rs crates/` on current main returns exactly
three sites:

| Site                                                                                                                                         | Verdict                                                                                                                                                                         |
| -------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `crates/thegn-svc/src/plugin/provider.rs:207` — `let provider_id: &'static str = Box::leak(format!("plugin:{plugin_id}").into_boxed_str());` | **FIX.** This is the issue's real target (it cites `:143`; the line moved).                                                                                                     |
| `crates/thegn-svc/src/forge/native.rs:581` — `ForgeError::NotConfigured(Box::leak(format!("no runtime: {e}").into_boxed_str()))`             | **FIX. The issue misses this one and it is worse.** It leaks on an _error path_ inside `graphql()`, so every failed runtime construction leaks again, unbounded across retries. |
| `crates/thegn-core/src/util.rs:1590` — `test_lane()`                                                                                         | **LEAVE.** Inside `#[cfg(test)]` (module starts line 1559) and deliberately documented: the production lane is process-wide, so tests need an owned one. Do not touch it.       |

`crates/thegn-host/src/plugin_providers.rs` no longer contains a `Box::leak` —
that half of the issue is already resolved. Confirm this yourself and record it
as already-met rather than inventing work.

## Primary decisions

- **`ForgeError::NotConfigured` is the blocker to think about.** It currently
  holds a `&'static str`. Fixing the leak means changing that variant's payload
  to an owned/`Cow`/`Arc<str>` form, which touches every construction and match
  site of that variant. That is the real work in this lane. Do it — a leak on a
  retryable error path is a genuine defect — but if the blast radius turns out
  to be larger than the plugin fix plus this variant, STOP and report a blocker
  with the site count rather than half-converting.
- **No global mutable registry and no interning map** as a substitute. That
  trades a leak for shared mutable state and a lock on a hot path. Ownership
  belongs to the adapter/router, per the issue's own constraint.
- **THE-165 is unlanded**; it is listed as a blocker but the leak fix does not
  depend on generation-tagging. Do not implement THE-165's session generations
  here.
- Prefer `Arc<str>` over `String` where the value is cloned across router
  clones and async calls; avoid adding a clone per call on a hot path.

## Tests required

- A rebuild-churn test: construct and drop the provider adapter/router many
  times and assert identity is still correct afterwards. A literal
  "thousands of iterations, stable RSS" memory assertion is flaky in CI —
  instead assert **no `'static` promotion remains** (the type no longer permits
  it) and cover identity correctness across a reload cycle. State this
  substitution in your artifact.
- The forge error path: assert the error carries the message without a
  `&'static str` payload.

## Validation you must NOT run

No cargo, builds, nextest, clippy. The primary runs the batch gate centrally.
Changing a public error enum may ripple into `thegn-host`; list every file you
touched so the primary can scope the build.
