# Persist the remote surface map and product-parity decisions

Linear: THE-98
Parent: THE-41

## Problem

A prior comment refers to ten remote-access findings, but no complete,
versioned map is attached to Linear or persisted in the repository. The parent
also mixes a broad reference-product feature list with cloud/storage links
without deciding what Thegn should own. That makes overlap analysis unreliable
and lets security/product findings disappear into prose.

## Proposed change

- Persist a versioned remote-access audit artifact and link it from THE-98 and
  THE-41.
- Inventory every remote surface and record owner, stability, auth/
  confidentiality boundary, extension seam, provider/platform support, failure
  behavior, tests, docs, and gaps.
- Recover or reconstruct the referenced ten findings and classify each with
  concrete evidence.
- Evaluate the full reference matrix as Shipped, Partial, Candidate, or
  Non-goal, with rationale rather than assuming parity.
- Link existing work and create bounded issues only for accepted gaps, each
  with owner, priority, dependencies, and acceptance criteria.
- Define a refresh trigger and accountable owner so the map remains useful.

## Non-goals

- Implementing any audited feature in this documentation change.
- Treating Termix/reference parity as a commitment.
- Selecting a cloud/storage dependency before an accepted capability requires
  it.
