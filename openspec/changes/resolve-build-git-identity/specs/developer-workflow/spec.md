## ADDED Requirements

### Requirement: Build identity follows the selected Git checkout
The build script SHALL identify the checkout being compiled and register existing
Git metadata inputs needed to invalidate that identity when HEAD changes.

#### Scenario: A commit changes only the branch ref
- **WHEN** the selected branch advances without changing application source
- **THEN** the next build refreshes its embedded commit identity

#### Scenario: An unchanged linked checkout is built twice
- **WHEN** the checkout and its Git identity are unchanged
- **THEN** the second build remains fresh without an invalid gitfile/HEAD watch

#### Scenario: A source archive is built
- **WHEN** Git metadata is unavailable
- **THEN** the build succeeds with empty commit identity and no missing Git watches
