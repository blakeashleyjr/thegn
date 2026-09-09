# Design

## Bidirectional delivery inventories

A checked-in, schema-validated change index records lifecycle, disposition, and
Linear/project owners for every active OpenSpec directory. A separate
issue-centric inventory enumerates every reviewed active delivery issue and
maps it to its OpenSpec change(s), or to an explicit `no-spec` rationale and
project. The issue inventory is checked in rather than derived from the change
index: otherwise a new issue with no change would be invisible by construction.

The validator requires exact back-references and identical project ownership in
both directions. Archived history is outside the active issue graph, so an active
issue cannot point at an archived index row and an archived historical owner does
not keep a closed issue artificially active. Both files contain no tokens and can
be checked offline. A delivered change records its delivery date and must archive
within the documented seven-day reconciliation window. Archive and issue-close
transitions are human decisions, but the two views make inconsistent combinations
visible.

## Repository-only gates

The gate validates paths and lifecycle invariants, runs strict OpenSpec
validation, detects references to archived changes as active work, and derives
plugin API version/supported extension points/control scopes from code or
generated snapshots rather than duplicated prose. Generated schema and
surface-gap ratchets remain their own specialized sources and run through
`cargo nextest`, the repository-standard test runner.

## External reconciliation

A report command combines the offline inventories with an optional, separately
exported Linear snapshot.
Without credentials it reports repository truth only and succeeds when local
invariants hold. With a snapshot it adds status, project, and issue-membership
drift. Detailed drift output is capped while retaining the full count. It never
opens a network connection or mutates Linear; maintainers apply the closure
checklist deliberately.
