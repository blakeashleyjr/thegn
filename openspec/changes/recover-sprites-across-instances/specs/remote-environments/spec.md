## ADDED Requirements

### Requirement: A sprite created by another thegn instance is recovered without a manual adopt

When a listed sprite has no ledger record under this scope, the Sprites
provider SHALL read the creation marker on the sprite and record the sprite as
ours only if the marker names this sprite and this account and either belongs to
this controller or its lease has lapsed. It SHALL refuse a marker that is
missing, unreadable, names another sprite or account, or holds a live lease for
another controller, and the refusal SHALL name `thegn env adopt`. It SHALL never
rewrite an existing ledger record.

#### Scenario: Dev reattaches to a sprite stable created

- **WHEN** the marker on a listed, unrecorded sprite names another controller
  on the same account and its lease has lapsed
- **THEN** reattach records the sprite as ours and does not create or delete it

#### Scenario: A live lease refuses

- **WHEN** the marker's lease for another controller has not lapsed
- **THEN** reattach refuses and the ledger is unchanged
