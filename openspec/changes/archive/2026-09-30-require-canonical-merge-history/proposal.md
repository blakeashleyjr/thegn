# Require canonical Git history for automatic merges

## Why

Replacement refs can forge already-merged results or cause fold plumbing to omit
candidate changes while recording candidate ancestry. The exact-commit gate then
tests a faithfully materialized but incorrect fold. Legacy graft files, including
ambient graft-file overrides, can independently forge cleanup ancestry.

## What Changes

- Add bounded, local-only canonical-history admission and original-identity
  revalidation across discovery, selected snapshots, fold preparation, gate
  outcomes, target advancement and automatic worktree cleanup.
- Refuse replacement refs, graft/shallow history, unknown metadata, unsupported
  history overrides and nonlocal transports lacking equivalent proof.
- Preserve infrastructure diagnostics, queue evidence and user history; never
  classify unsafe history as a branch failure or silently repair metadata.
- Retain the documented limitation that observations are not atomic with an
  external same-UID writer temporarily changing history during a command.

## Impact

- Owner: THE-606, Runtime Security & Host Architecture; blocks THE-586 delivery.
- Depends on THE-597 bounded probe/platform pins, and existing THE-588/THE-600
  cleanup plus THE-591/THE-595 snapshot/outcome semantics.
- Remote/provider and shallow-history automatic operations become explicit
  infrastructure holds until equivalent trustworthy history proof is available.
- No schema, global Git provider semantics, automatic ref cleanup, host service
  installation or live repository repair is introduced.
