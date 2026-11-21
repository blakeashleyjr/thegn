## Added Requirements

### Requirement: Pinned directory listings use descriptor-relative streams

On macOS, directory entries requested through a held directory descriptor
SHALL be enumerated from a fresh stream opened relative to that descriptor,
without resolving a replacement path or following a symlink. Every listing
SHALL have an independent read position. Other Unix platforms SHALL preserve
the same pinned-directory behavior, and non-Unix listing behavior SHALL remain
path-based.

#### Scenario: Managed-home rollback can list the newly created home

- **WHEN** establishing an owner manifest fails for a newly created managed
  account home on macOS
- **THEN** rollback lists that home through its held descriptor and removes it
  only when it is empty after temporary manifest cleanup

#### Scenario: Repeated listings start from the beginning

- **WHEN** a pinned directory is listed more than once
- **THEN** each listing returns the current entries, even when the earlier
  listing consumed its stream
