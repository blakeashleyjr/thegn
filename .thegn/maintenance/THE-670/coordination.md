# Coordination brief — THE-670 (primary)

## Scope

`crates/thegn-svc/src/forge/native.rs` and its test files, `deny.toml`,
`Cargo.toml`. Nothing else.

## What must not change

This is a **transport swap behind an unchanged seam**, not a rewrite. The three
parsers (`parse_graphql_pr_list`, `parse_graphql_pr`, `parse_graphql_pr_status`)
are pure, fixture-tested and operate on `serde_json::Value` — **they should not
need to change at all.** If your plan modifies a parser, say why; that is a
signal the swap has leaked into the logic.

Equally: the circuit breaker, the 20s per-request timeout and `resolve_token` are
already thegn's own code. Preserve their behaviour exactly. The issue notes
THE-622 and THE-621 describe behaviour to **preserve**, not work to do.

## The part that will actually be fiddly

Error classification. Today `classify_error` matches octocrab's error enum
(`Graphql | GitHub { source } | Service | Hyper`). `TrackerHttpClient` has a
different error shape, so the mapping has to be re-established deliberately —
and a GraphQL response that is HTTP 200 with an `errors` array is **not** a
transport error. Getting that wrong silently converts a GraphQL failure into a
success with empty data, which is the defect class this repo keeps finding.

**Enumerate the old classification's cases and show what each maps to.** That
table is the review-worthy part of the plan.

## Constraints

- **Vendor CLIs and vendor SDKs live only in their implementation files.** There
  is a shrink-only ratchet on `gh` calls outside the forge impl.
- `thegn-svc`'s HTTP client already owns the concurrency semaphore
  (`MAX_IN_FLIGHT`), per-operation deadline, and `MAX_BODY_BYTES`. **Use them; do
  not add a second policy layer.** A GraphQL body over the cap must fail as a
  bounded error, not allocate.
- Removing the RUSTSEC waiver from `deny.toml` is part of the change. **Confirm
  the waiver is no longer needed by checking nothing else pulls the advisory's
  crate** — removing it while another path still depends on it turns a documented
  waiver into a red `deps-audit`.
- No network in tests. Fixture-driven only.

## Deliverable

A plan the primary reviews and greenlights before implementation. No production
edits in this stage.
