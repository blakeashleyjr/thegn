## ADDED Requirements

### Requirement: Standalone host capture SHALL distinguish source absence from refusal

An internal capture operation SHALL return absence only for a genuinely missing
path component observed and reverified within its supported namespace, with no
orphaned sidecar beside a missing base. Existing unreadable,
unsupported, malformed or incompatible state SHALL produce a typed refusal,
never a successful empty registry. It SHALL NOT initialize or migrate state.

#### Scenario: Missing private state file

- **WHEN** inspection reaches a missing component in an otherwise supported namespace
- **THEN** capture reports absence without creating a file or parent directory.

#### Scenario: Absence changes before publication

- **WHEN** a retained ancestor changes or the first missing component is created before final verification
- **THEN** capture refuses the changed observation without reading SQL or publishing absence.

#### Scenario: Sidecar without a base

- **WHEN** the base is missing but an adjacent WAL, SHM or journal object exists at verification
- **THEN** capture returns a typed refusal without creating a base or deleting the orphan.

#### Scenario: Unreadable source

- **WHEN** a supported existing parent cannot be searched or its database cannot be read
- **THEN** capture returns a typed inspection/open refusal rather than absence, and a fresh capture can succeed after permissions are restored.

#### Scenario: Invalid existing state

- **WHEN** an existing DB contains incompatible version or malformed host definitions
- **THEN** capture returns a typed error without silently selecting a default registry.

### Requirement: Standalone capture SHALL preserve WAL reads and state its limits

The operation SHALL use normal read-only SQLite locking and current WAL state,
allowing SQLite-managed sidecar activity. It SHALL NOT substitute immutable or
procfd-only main-file reads. Platform observations SHALL be separate from launch
authorization and SHALL NOT claim hostile same-UID path replacement resistance.

#### Scenario: Uncheckpointed committed host definition

- **WHEN** a supported private WAL database has a committed uncheckpointed host row
- **THEN** capture reads it through the strict same-transaction snapshot operation.

#### Scenario: Unsupported or changed namespace

- **WHEN** Linux inspection observes a symlink, unsafe type or writable ancestor,
  unsupported filesystem, or identity change
- **THEN** capture refuses without falling back to a tolerant opener.

#### Scenario: Component not yet integrated

- **WHEN** only the standalone capture component is installed
- **THEN** it does not activate startup or launch authority, worker deadlines,
  global policy installation or receiver behavior.
