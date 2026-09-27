# Primary review + greenlight — THE-670

Reviewing row 634. **APPROVED, with all four conflicts resolved below. One of them
is my brief being wrong — read conflict 1 first.**

This is the strongest investigation of the batch. The error-classification table
is the deliverable: you extracted the **exact** old behaviour including the two
rows most likely to be silently dropped — `403` with "rate limit" in the message
mapping to `RateLimited` rather than `NotAuthenticated`, and "any other SDK error
variant is treated as not reached but does **not** increment the circuit". Preserve
that table row for row; it is the acceptance criterion in practice.

## CONFLICT 1 — keep 10 seconds. My brief was wrong.

My coordination brief said "the 20s per-request timeout", copying the issue text.
You checked the branch and found `OCTOCRAB_REQUEST_TIMEOUT` set to **10 seconds
with documented reasoning** (`native.rs:412-415`).

**The branch wins. Preserve 10 seconds**, including while waiting for the shared
permit and during connect, TLS and read. `TrackerHttpClient`'s 20-second default
must be overridden, not inherited — silently doubling the ceiling would lengthen
every stalled refresh, which is a regression on the interactive path and exactly
what that documented comment exists to prevent.

Thank you for not taking the brief's word for it. That check was the difference
between a clean swap and a quiet regression.

## CONFLICT 2 — scope expansion APPROVED: `crates/thegn-svc/src/issue/http.rs` is in scope

You correctly identified that `TrackerHttpOperation::json` discards error bodies
and maps every 403 to `Auth`, so reusing it as-is **cannot** preserve the
403-rate-limit distinction or the other-status message classification.

Of your three options, take the first: **add a small bounded response-envelope
operation to `issue/http.rs`** (status plus bounded JSON/body) and let `native`
classify the response. I am explicitly widening the scope to include that file.

Reasoning, so it is not revisited: losing the rate-limit distinction is a
user-visible regression — "you are rate limited" and "you are not authenticated"
demand completely different responses from an operator — and standing up a second
reqwest client would duplicate the client policy, which is precisely what this
repo's provider-seam rule forbids. A bounded additive operation is the small
change.

Two conditions on it:

1. **The new operation keeps every existing check** — redirect refusal, encoding,
   content-type, body-size cap, and the absolute per-operation deadline. It is a
   different _shape_ of result, not a weaker policy.
2. **`json()`'s behaviour does not change**, and no existing tracker caller
   (Jira, Linear, the rest) changes behaviour. Add beside it; do not refactor it.

The production operation-deadline override is likewise approved, subject to
conflict 1's 10-second value.

## CONFLICT 3 — `Cargo.lock` and the audit doc are both in scope

`Cargo.lock` obviously must change; my file list omitting it was an oversight.
`docs/audits/dependency-audit-2026-09-15.md` is also in scope — the acceptance
criteria ask for a remeasured dependency count and that is a recording change,
not scope creep. Record the **measured** count closure, not the issue's predicted
−31.

## CONFLICT 4 — accepted, and state the limit plainly

A real TLS-handshake stall is not reproducible in a no-network fixture. Verify the
absolute operation deadline and the circuit behaviour with a **stalled local
response** from the Axum fixture, and say in the report which connect-timeout
behaviour could only be confirmed by inspection. Do not describe an inspected
constant as a tested one.

## The RUSTSEC waiver — the order matters

Remove the waiver and its explanatory comment **only after** static lockfile and
dependency-tree confirmation that nothing else reaches `rsa` or the advisory
chain. Removing it while another path still depends on it converts a documented,
reasoned waiver into a red `deps-audit`, which is strictly worse than leaving it.
Show the tree output in your report.

## Confirmed unchanged

Query strings, the three pure parsers, gate and checkout-scope validation, token
precedence, `GithubCli`, and the forge seam. `GithubNative` stays synchronous on
its current-thread runtime. If a parser needs to change, that is a signal the swap
has leaked into the logic — stop and report it.

Reuse `TrackerHttpBudget::process()`, the existing no-redirect client,
response-size enforcement and the per-operation deadline. **No parallel semaphore
and no second client policy.**

## Validation

Attempt `nix develop --command cargo check -p thegn-svc --all-targets` and narrow
`cargo nextest run -p thegn-svc native`. **The pipeline sandbox mounts
`/nix/store` read-only, so this usually fails outright** — say exactly that and
stop if it does. The primary runs clippy, the full workspace nextest and smoke
centrally.

Never report a verdict for code you could not compile; state what you could not
run. If a further conflict contradicts a decision above, say so — conflict 1
proved that worth doing.
