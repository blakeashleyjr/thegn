# Primary review + greenlight — THE-316

Reviewing row 526's investigation (`.thegn/pipeline/THE-316/maintenance-investigate/526.md`).

**Verdict: APPROVED to implement.** The investigation correctly confirmed that
the issue's headline defect (`.await.unwrap_or(serde_json::Value::Null)`) was
already fixed by `c6eaa4d0`, and correctly narrowed the lane to the two gaps
that genuinely remain:

- `jira.rs:636-642` — the final re-fetch never verifies the issue reached the
  requested status category.
- `jira.rs:574-634` — status is applied before title with no partial-field
  reporting.

Record the already-fixed criterion as **met, with evidence** in your
implementation artifact. Do not re-fix it.

## Decision 1 — typed partial-update error, not a bounded `Api` string

Use a **structured typed** error. The acceptance criterion asks for "an explicit
per-field result / idempotent retry contract", and a caller cannot act on a
formatted `Api(String)` blob: it has to know _which_ fields landed to retry
safely. That is the entire point of the change.

Scope guard: add **one** variant to the existing `IssueError` enum carrying the
applied and unapplied fields. Do not introduce a new error type hierarchy, and
do not restructure `IssueError`'s other variants.

## Decision 2 — production order is title-then-status; the test matrix follows it

You are right that "test both orders" and "apply title first" cannot both hold.
**Title-then-status is the production order.** Reason: the title PUT is
cosmetic, idempotent and reversible, while a status transition can fire workflow
automations, notifications and timers and may have no reverse transition
available. Doing the irreversible operation **last**, only after the cheap one
has succeeded, minimises the damage of a partial write.

My earlier "both orders" wording is superseded. The matrix is:

1. **Title fails** → the status transition is never attempted; assert no
   transition request was issued and nothing was applied. This is a clean
   failure, not a partial one.
2. **Title succeeds, status fails** → the typed partial error names title as
   applied and status as not applied.
3. **Both succeed** → verification actually runs and the requested category is
   confirmed.

Plus the provider-level cases already listed: HTTP error, empty/204 transition
response, unavailable transition, stale transition id.

## Verification semantics

After a successful transition, the existing final re-fetch must assert the
fetched status **category** matches the requested one (the same
`new`/`indeterminate`/`done` mapping used to select the transition). A 2xx
transition that leaves the issue elsewhere is a failure, not a success. Do not
add a retry/poll loop for eventual consistency in this lane — if the category
does not match on the existing fetch, report it. Note any eventual-consistency
concern as a follow-up finding instead.

## Restated constraints

- Never put a raw provider body in an error. Bound the context and keep
  tokens/credentials out; reuse the crate's existing redaction/bounding helper
  rather than writing a second one.
- Scope is `crates/thegn-svc/src/issue/jira.rs` plus its tests. THE-126 /
  THE-164 / THE-195 / THE-319 are unlanded — do not build the generic
  capability model here.
- Use the crate's existing HTTP test seam. No network calls in tests.

## Validation

Do not run cargo/nextest/clippy. The primary runs the batch gate. Adding an
`IssueError` variant may ripple into `thegn-host` match sites — list every file
you touch so the primary can scope the build, and flag it if
`docs/api/control-v1.json` is affected.
