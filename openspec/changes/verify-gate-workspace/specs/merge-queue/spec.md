## ADDED Requirements

### Requirement: Gate only an exclusively admitted exact local checkout

A nonempty local gate MUST establish supported locking and physical linked-Git
identity before preparation. Admission failures MUST be infrastructure errors,
not branch failures or permission to continue unlocked. A reused gate MUST NOT
recursively purge an unknown path or globally prune registrations for repair.
Gate-owned Git operations and the inherited setup/gate Git environment MUST use
the original objects of pinned OIDs, not mutable replacement objects, without
editing the user's replacement refs or configuration.

#### Scenario: Another gate owns the reusable checkout

- **WHEN** nonblocking lock acquisition reports contention or failure
- **THEN** the request holds without changing checkout HEAD or running its gate.

#### Scenario: Gate path, registration or root repository is substituted

- **WHEN** physical identity or a retained Git association cannot be verified
- **THEN** the request holds and preserves unknown content for explicit inspection.

#### Scenario: Setup or gate changes the commit

- **WHEN** a configured command exits successfully but HEAD no longer matches
  the exact requested commit
- **THEN** the result is infrastructure error and cannot authorize target advance.

#### Scenario: Throwaway cleanup is uncertain

- **WHEN** the owned child identity cannot be revalidated after execution
- **THEN** no recursive fallback cleanup is attempted and the path is retained.

### Requirement: Fresh tracked-entry materialization under retained ownership

Before checkout and before gate execution, sparse configuration and hidden index
flags MUST be refused as infrastructure. They MUST NOT be cleared automatically. Read-only
probes MUST share the bounded cleanup/gate capture capacity; inability to complete
a bounded probe MUST NOT be interpreted as safe materialization. Tracked entries
MUST be materialized using a fresh private index populated from the pinned commit,
without replacing the real index. Configured gate filters retain existing operator
authority; cleanup's filter policy MUST NOT be relaxed as a consequence.

#### Scenario: Cached stat metadata hides stale bytes

- **WHEN** ordinary index tags and checkout report no change despite stale bytes
- **THEN** fresh-index materialization restores the parent tracked entries before
  configured setup, preserving ignored/untracked artifacts and the real index.

#### Scenario: Materialization child wait becomes unknown

- **WHEN** the owned blocking child wait returns an error
- **THEN** the process poisons further materialization and retains the entire
  child/workspace/index lease rather than returning a writable checkout to reuse.

This does not claim supervision of detached operator-filter descendants,
recursive submodule preparation or byte-for-byte immutability after authorized
filter/setup transformations. Tracked mtimes may change on every attempt.
