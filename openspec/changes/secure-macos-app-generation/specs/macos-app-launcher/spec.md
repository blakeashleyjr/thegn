## ADDED Requirements

### Requirement: macOS app generation is pinned and atomic

`packaging/macos/make-app.sh` SHALL pin its canonical destination directory
before filesystem effects and SHALL perform all generated-bundle operations
relative to that pinned directory. It SHALL use an atomic no-replace operation
for first installation and an atomic directory exchange for upgrades. Before
an exchange it SHALL revalidate that the existing bundle is generator-owned.
It SHALL require Python 3.9 or newer before destination mutation and fail with a
clear diagnostic if Python or the host atomic rename primitive is unavailable.

#### Scenario: Destination path changes during generation

- **WHEN** the destination pathname is replaced after the generator opens it
- **THEN** generated files remain relative to the originally opened directory
- **AND** the replacement path and its target are not modified

#### Scenario: Existing app path changes before promotion

- **WHEN** the app path is replaced with a foreign directory or symlink after
  preflight and before promotion
- **THEN** the generator refuses the exchange
- **AND** preserves the foreign path and all of its bytes
- **AND** safely removes only its own marked staging bundle

#### Scenario: Atomic replacement is unavailable

- **WHEN** the host does not provide a safe atomic exchange operation
- **THEN** an upgrade fails closed before changing the existing app path
- **AND** the previous complete bundle remains intact

#### Scenario: Retirement of the displaced bundle fails after exchange

- **WHEN** a verified atomic exchange has installed the new complete bundle
  but safe removal of the old generation fails
- **THEN** the new complete bundle remains active
- **AND** the generator reports and preserves the old cleanup residue without
  swapping a partially removed old generation back into the app path
