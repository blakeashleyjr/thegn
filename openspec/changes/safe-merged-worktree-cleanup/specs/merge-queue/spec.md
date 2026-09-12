## ADDED Requirements

### Requirement: Automatic collection verifies local ownership

Automatic merged-worktree collection SHALL verify registered local linked
checkout identity, common repository, branch tip and current target ancestry
before any lifecycle side effect. Unknown or inconsistent evidence SHALL refuse.

#### Scenario: A global queue contains unrelated worktrees

- **WHEN** expiry collection runs for one repository and target
- **THEN** foreign, main, unknown, remote or mismatched paths remain untouched
- **AND** their retry records remain and are not reported as collected.

### Requirement: Automatic removal preserves uncertain user work

Automatic removal SHALL use no force or recursive fallback and SHALL revalidate
after hooks/teardown. Dirty, untracked, ignored, unreadable, filter-driven or
hidden-index state SHALL prevent automatic removal.

#### Scenario: State changes while cleanup is preparing

- **WHEN** a branch, repository identity or cleanliness check changes
- **THEN** automatic cleanup refuses and retains its retry record.

### Requirement: Automatic collection retains branches until ref-type proof exists

Automatic collection SHALL NOT mutate source, target or other branch refs.
When physical removal succeeds but branch deletion was requested, it SHALL
retain a typed queue hold without changing the original landing timestamp.

#### Scenario: A source or target becomes a symbolic ref

- **WHEN** a source or target ref is replaced by a same-OID symbolic ref
- **THEN** automatic cleanup does not delete the symbolic ref or its referent.

#### Scenario: Reconciliation sees a held worktree already removed

- **WHEN** generic cache pruning or a later sweep observes the absent worktree
- **THEN** the recognized landed hold survives until explicit queue dismissal
- **AND** it is not counted as newly collected or cleared.

### Requirement: Collection reports actual outcomes

Only verified physical removal SHALL be reported as collected. Refusal, dirty
state and partial bookkeeping failure SHALL be distinguishable and SHALL not
silently discard retry records.

#### Scenario: Removal cannot acquire a claim or Git refuses

- **WHEN** an automatic collection attempt cannot complete verified removal
- **THEN** the worktree is not counted as collected and its queue row remains.
