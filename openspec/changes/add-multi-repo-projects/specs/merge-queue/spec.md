# Merge Queue

## ADDED Requirements

### Requirement: A project feature can be enqueued across member repos as independent per-repo rows

thegn SHALL let a user enqueue a project feature in one action
(`merge add --project <p> --feature <branch>`): the feature set is resolved
and each member's branch is recorded in **that repo's own per-repo queue**
as an ordinary independent row, with a per-member outcome report (queued /
no such branch in this member / ineligible). Queues remain strictly
per-repo: draining, gating, and landing are unchanged and MUST NOT acquire
cross-repo ordering or atomicity from this action — a cross-repo land is
never presented as atomic.

#### Scenario: Batched enqueue fans out per repo

- **WHEN** `merge add --project shop --feature tg/x` is run and members
  `api` and `web` have the branch while `shared-lib` does not
- **THEN** `api`'s and `web`'s queues each gain an independent row for
  `tg/x`, `shared-lib` is reported as having no such branch, and no
  cross-repo ordering is recorded

#### Scenario: Draining stays per-repo

- **WHEN** the queues are drained after a batched project enqueue and one
  member's branch is deferred on a conflict
- **THEN** the other members' branches drain and land under their own
  repos' rules, unaffected by the deferred member
