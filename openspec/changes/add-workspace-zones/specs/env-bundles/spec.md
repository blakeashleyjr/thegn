# Environment bundles

## ADDED Requirements

### Requirement: A zone-owned bundle is a credential sub-vault

A bundle MAY declare an owning zone. A zone-owned bundle SHALL be composable only
by a worktree that belongs to that zone; a foreign or unzoned worktree that would
compose it (whether bound directly, at the workspace or global scope, or reached
via another bundle's `extends`) has it skipped and the denial recorded, and the
launch continues without it. A global (unzoned) bundle remains composable
everywhere.

#### Scenario: A foreign worktree is denied a zone-owned bundle

- **WHEN** a worktree in zone `clientB` composes a bundle owned by zone `clientA`
- **THEN** the bundle is skipped, the denial is recorded, and its env is not
  applied

#### Scenario: A member composes its own zone's bundle

- **WHEN** a worktree in zone `clientA` composes a bundle owned by `clientA`
- **THEN** the bundle's env is applied

#### Scenario: extends reachability is covered

- **WHEN** a visible bundle `extends` a foreign zone-owned bundle
- **THEN** the visible bundle folds but the foreign parent is denied

#### Scenario: A global bundle stays usable inside a zone

- **WHEN** a worktree in a zone composes a bundle with no owning zone
- **THEN** the bundle's env is applied
