# Git backend

## ADDED Requirements

### Requirement: Submodule state is a bounded provider read

When strict `.gitmodules` metadata is present, the service Git adapter SHALL
derive registered/initialized/dirty/conflicted gitlink state, recorded pointer
changes, and direction/summary facts from bounded local Git operations. The
read MUST be skipped when metadata is absent and MUST NOT fetch missing
submodule commits.

#### Scenario: Metadata-free repositories pay no submodule read

- **WHEN** a repository has no `.gitmodules`
- **THEN** submodule state is empty without running recursive submodule reads

#### Scenario: Missing history degrades

- **WHEN** a pointer target is not available locally
- **THEN** the adapter returns the pointer change with an unknown/bare-SHA
  summary and performs no network fetch

### Requirement: Gitlinks are atomic patch entries

Mode `160000` and `Subproject commit` records SHALL classify a change as a
submodule. A partial gitlink line selection MUST be rejected; a whole-entry
selection SHALL use atomic stage/restore semantics rather than a partial text
patch.

#### Scenario: Half a pointer cannot be staged

- **WHEN** a caller selects only one line of a submodule pointer change
- **THEN** selection validation rejects it before invoking Git
