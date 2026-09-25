# Primary review + greenlight — THE-170

Reviewing row 531's investigation (`.thegn/pipeline/THE-170/maintenance-investigate/531.md`).

**Verdict: APPROVED to implement.** The three-point fix is accepted as written,
with the payload choice decided below.

The investigation confirmed the primary's inventory: the real sites are
`plugin/provider.rs:207` and `forge/native.rs:581`, the host registry leak is
already resolved, and the `test_lane` leak in `util.rs` is test-only and stays.

## Blast radius — accepted, with a condition

The plan reports **8 files / 55 references**. The primary's brief said to stop
and report if this grew beyond the plugin fix plus the error variant, and you
did report it — correctly. Having seen the shape, **proceed**: the 55 references
are mechanical constructor/match-site updates of one enum variant, not 55
semantic decisions, and the lane touches files no other lane in this batch is
working in (the others are in `util.rs`/`doctor.rs`/`agent.rs`, the calendar
tree, `jira.rs`, `worktree_launch.rs`, `test_support/ratchet.rs`, the hello
plugin, and the hydration tickers).

**Condition:** if the count grows materially beyond ~55 once you start, or if
any site needs a _semantic_ decision rather than a mechanical conversion, STOP
and report rather than pushing through.

## Decision — `Cow<'static, str>` for `ForgeError::NotConfigured`

Use `Cow<'static, str>`, your preferred shape. Existing literal diagnostics stay
borrowed (no allocation for the common case), and only the runtime-build message
in `graphql()` becomes owned — which is precisely the leak. `Arc<str>` would
force every literal into an allocation for no benefit here, since these payloads
are not shared across threads after construction.

## Decision — the `provider_id` trait change is approved

Changing `IssueBackend::provider_id` from `&'static str` to `&str` is approved.
It stays object-safe, built-ins keep returning their static literals unchanged
(a `&'static str` coerces to `&str` freely), and it is the change that makes the
`'static` promotion impossible to reintroduce — which is the real deliverable.

Keep the signature change to exactly that. Do not take the opportunity to
restructure `IssueRouter`, `provider_ids`, or `list_per_provider` beyond what
the lifetime change forces.

## Tests — the compile-time contract is the guard, as agreed

Your substitution is accepted and was pre-approved in the coordination brief: no
RSS threshold (flaky), instead the `provider_id -> &str` contract plus a
construct/drop churn test asserting each generation keeps its exact
`plugin:<id>` identity and that replacement still routes correctly. Add the
forge regression asserting the dynamic message survives through the owned
payload.

## Restated constraints

- No global mutable registry, interning map, or generation tag. THE-165 is
  unlanded and is not a prerequisite — do not implement it.
- `util.rs` `test_lane` is untouched.
- Update the stale host comment as point 3 says.

## Validation

Do not run cargo/nextest/clippy. The primary runs the batch gate centrally.
Because this changes a public trait signature and an error variant, **list every
file you touch** in your report so the primary can scope the compile, and flag
any exhaustive-match breakage you could not resolve mechanically.
