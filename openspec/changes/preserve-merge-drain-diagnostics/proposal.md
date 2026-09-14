# Preserve merge-drain diagnostics

## Why

THE-608: the single-item drain passes diagnostic prose into the store's conflict
path argument, leaving error detail empty. Its panel reconstructs metadata from
ambiguous status text, retains stale fields and can summarize gate-error-only
results as success. THE-610: isolation admission refusal continues the same
attempt loop without consuming a budget or making progress.

## What Changes

Add typed exact replacement of status metadata without changing the existing
partial-update API. Carry the same fields through drain progress to the panel;
keep human status text separate from full OIDs, paths and diagnostics. Stop an
isolation-held item's retry loop without dispatching an agent or blaming source.

## Impact

Core store/SQLite and host merge-driver/panel behavior. No schema migration,
network permission, new automatic retry, lifecycle redesign or global setting.
Companion to THE-606 canonical-history admission and THE-589/591 outcome work.
Local merge delivery remains blocked on exact focused/full and native proof.
