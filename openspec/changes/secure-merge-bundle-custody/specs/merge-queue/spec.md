## ADDED Requirements

### Requirement: Own private cross-host bundle storage through fetch

The local merge-queue bundle receiver SHALL create an unpredictable private directory and an exclusive regular bundle leaf, retaining their identities until the actual Git fetch completes or fails. Unix directories and leaves SHALL be created with owner-only modes. Windows directories SHALL receive a protected owner-only DACL at creation and retain no-follow handle identity. Existing paths SHALL NOT be adopted or overwritten.

#### Scenario: Existing or path-like hostile leaf cannot be adopted

- **WHEN** an existing regular file, hard link, symbolic link, directory or FIFO occupies a proposed leaf
- **THEN** exclusive creation refuses it without overwriting or blocking on it

#### Scenario: Fetch succeeds or fails

- **WHEN** a private bundle is consumed by the actual Git fetch command
- **THEN** its owner remains alive through command completion and performs checked nonrecursive cleanup on success and failure

### Requirement: Preserve replacements during bundle cleanup

Cleanup SHALL compare retained parent and leaf identities before removing an entry, SHALL remove only the owned leaf and empty owned directory, and SHALL surface cleanup failures on the primary path. Failed construction SHALL retain descriptor custody through fallible post-create work. The implementation SHALL NOT claim an atomic filesystem lease against arbitrary same-user replacement.

#### Scenario: An observed identity changes

- **WHEN** a parent or leaf no longer matches its retained identity
- **THEN** cleanup refuses to delete the replacement and reports the unresolved cleanup

#### Scenario: Writing, flushing or construction fails

- **WHEN** a failure occurs after exclusive creation
- **THEN** the retained descriptor is closed and cleanup checks ownership without recursive removal or pathname-only fallback
