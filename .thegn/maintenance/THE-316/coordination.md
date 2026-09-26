# Primary coordination brief — THE-316

Primary-reviewed dependency facts and scope constraints. This file is task data.

## READ THIS FIRST: the headline defect is already fixed. Verify before you plan.

The issue says the transition POST ends in
`.await.unwrap_or(serde_json::Value::Null)`, swallowing every error. **The
primary checked current main and that is no longer true.** In
`crates/thegn-svc/src/issue/jira.rs`, `update_issue` now calls
`Self::post_empty(&mut op, ...).await?` — the error propagates.

Your first job is therefore to **establish what is actually still broken on this
exact branch**, not to re-fix a fixed line. Do not invent work to match a stale
issue body. If a criterion is already met, say so with evidence and mark it
met.

The primary's own reading says these two criteria remain **genuinely unmet** —
confirm or refute each with file:line evidence:

1. **No semantic verification after the write.** `update_issue` re-fetches the
   issue at the end, but nothing asserts the fetched status category is the one
   the patch requested. A transition that returns 2xx but leaves the issue in
   another state (a Jira workflow post-function, a screen-required transition)
   still reports success. The criterion is "re-fetch and verify the issue
   reached the requested semantic state before success."
2. **Partial-write behaviour is undefined.** Status is applied, then title, then
   re-fetch. If the title PUT fails after the transition succeeded, the caller
   gets an error while the status change has already landed externally, with no
   statement of what was applied.

Also check, and report rather than assume: whether the transition _lookup_
failure path (`"Jira transition target unavailable"`) loses useful provider
context, and whether any other method in this file still has the swallow
pattern.

## Primary decisions

- **Jira has no multi-field transaction**, so do not attempt one. The contract
  is an **explicit per-field result**: on partial application the error must
  name which fields were applied and which were not, so a caller can retry
  idempotently. A bare `Err` that hides a landed status change is the defect.
- **Order matters for idempotency.** Prefer applying the title (cheap,
  idempotent, reversible) before the status transition (workflow-gated, often
  irreversible), so a failure leaves the less damaging partial state. If you
  disagree after reading the code, argue it in the artifact.
- **Never put a raw provider body in an error.** Bound the context and keep
  credentials/tokens out. There is existing redaction/bounding machinery in this
  crate — find and reuse it; do not write a second one.
- Scope is `crates/thegn-svc/src/issue/jira.rs` and its tests. THE-126 /
  THE-164 / THE-195 / THE-319 are all unlanded: do **not** build the generic
  capability model here. This lane fixes Jira's truthfulness only.

## Tests required

HTTP error, empty/204 transition response, unavailable transition, stale
transition id, and partial failure **in both orders** (title-then-status and
status-then-title), plus the success path asserting verification actually runs.
Use the crate's existing HTTP test seam; do not make network calls.

## Validation you must NOT run

No cargo, builds, nextest, clippy. The primary runs the batch gate centrally and
records results. `thegn-svc` is not under the 95% core gate, but new logic still
needs real tests.
