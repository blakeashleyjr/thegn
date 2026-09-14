## ADDED Requirements

### Requirement: Automatic merge operations require canonical local history

Automatic candidate preparation, fold results and destructive merged-worktree
cleanup SHALL require an admitted local history observation. Admission SHALL
refuse replacement refs, legacy graft metadata, shallow metadata, unsupported
ambient history overrides, unknown metadata and unsupported nonlocal placement.
Admission SHALL NOT alter user refs, configuration or history to obtain proof.

#### Scenario: Replacement history would forge a no-op

- **WHEN** replacement refs make a candidate appear already merged
- **THEN** automatic integration reports infrastructure unavailability
- **AND** it does not publish UpToDate or advance the target

#### Scenario: A callback changes history before its verdict

- **WHEN** a merge, snapshot or gate callback changes observed history
- **THEN** original-identity revalidation refuses continuation
- **AND** an ordinary failure is not published as candidate blame
- **AND** earlier authorized callback side effects may remain without rollback

#### Scenario: Destructive cleanup cannot verify ancestry semantics

- **WHEN** canonical history admission or later revalidation fails
- **THEN** automatic cleanup retains worktree, source refs and queue evidence
- **AND** it reports the refusal rather than a successful collection

#### Scenario: Nonlocal transport lacks canonical-history proof

- **WHEN** an automatic operation targets a remote or provider Git location
- **THEN** it reports unsupported infrastructure before contacting that transport
- **AND** a local repository observation is not substituted for remote authority

### Requirement: History observation limitations remain explicit

The implementation SHALL retain original admitted identities across callbacks
and SHALL revalidate before publishing no-op or gate evidence and before target
advancement or physical removal. It SHALL NOT claim atomic isolation from an
external same-UID writer or treat a fixed query deadline as a bound on OS calls.

#### Scenario: External writer race remains outside the observation contract

- **WHEN** another process changes and restores history between observations
- **THEN** documentation identifies that race as unproven rather than claiming
  an atomic filesystem or Git transaction
