## MODIFIED Requirements

### Requirement: Degradation ladders and multi-account routers are reusable

`thegn-svc` SHALL provide a `Ladder` that runs an operation across ordered layers (native → CLI → unavailable) and returns the first non-fall-through result, and a `Router` that fans an operation out across configured accounts, merging successes and isolating a single account's failure so it never discards the others' results. Account-shaped routers SHALL accept dynamically-provided backends (`push_backend`), so a provider implemented outside the binary — a plugin over the `provider.call` bridge — composes exactly like a configured account.

#### Scenario: Ladder falls through on an unsupported layer

- **WHEN** the first layer of a ladder returns an error of class `Unsupported` or `NotInstalled` and the second returns `Ok`
- **THEN** the ladder returns the second layer's `Ok`

#### Scenario: Ladder stops on a final error

- **WHEN** the first layer returns an error of class `Auth`
- **THEN** the ladder returns that error without consulting later layers

#### Scenario: One failing account does not poison a fan-out

- **WHEN** a router fans out across three accounts and one returns an error
- **THEN** the merged result contains the two successful accounts' items and the failure is logged

#### Scenario: A plugin backend routes like an account

- **WHEN** a plugin issue provider is registered and the router fans out
- **THEN** its results merge under its account label, and its failure is isolated like any account's
