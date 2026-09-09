# Design

## Delivery index

A checked-in, schema-validated index maps an active delivery issue to its Linear
identifier, owning project, OpenSpec change (or explicit `no-spec` rationale),
and lifecycle state. It contains no tokens and can be checked offline. Archive
and issue-close transitions are human decisions, but the index makes inconsistent
combinations visible.

## Repository-only gates

The gate validates paths and lifecycle invariants, runs strict OpenSpec
validation, detects references to archived changes as active work, and derives
plugin API version/supported extension points/control scopes from code or
generated snapshots rather than duplicated prose. Generated schema and
surface-gap ratchets remain their own specialized sources.

## External reconciliation

A report command combines the offline index with optional Linear read access.
Without credentials it reports repository truth only and succeeds when local
invariants hold. With Linear it adds status/project/priority drift. It never
mutates Linear; maintainers apply the closure checklist deliberately.
