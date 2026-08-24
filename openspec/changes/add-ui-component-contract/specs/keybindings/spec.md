# keybindings — delta for add-ui-component-contract

## ADDED Requirements

### Requirement: Zone-local keys are one table shape that drives dispatch

Zone-local key handling SHALL use one shared table shape — chord, label, hint tier, dispatch discriminant, zone id — as `sidebar_keytable` does today, and that one table SHALL feed dispatch, the statusbar hint strip, which-key, and `keymap_merge::collect`, so a zone key cannot exist without surfacing and a hint cannot drift from what the key does. Panel section keys MUST migrate from hint-only declarations to this shape, with the `run.rs` per-section dispatch match replaced by table lookups and the source-text drift test (`hint_table_matches_dispatch`) deleted — the drift it guarded becomes unrepresentable. `thegn keys list` and the generated keybindings help page SHALL attribute every zone-table binding to its real zone rather than a hardcoded `global`. `ActionSpec` remains the registry for global rebindable actions; zone-table keys remain non-rebindable in this change.

#### Scenario: A new section key surfaces everywhere from one datum

- **WHEN** a panel section adds a key to its table with a chord, label, tier and dispatch discriminant
- **THEN** the key dispatches, appears in the section's hint strip at its tier, and shows under the section's zone in `thegn keys list` — with no second declaration anywhere

#### Scenario: A hint cannot advertise a dead key

- **WHEN** a section table entry's dispatch discriminant has no handler arm
- **THEN** the build fails on the unhandled discriminant, instead of shipping a hint for a key that does nothing
