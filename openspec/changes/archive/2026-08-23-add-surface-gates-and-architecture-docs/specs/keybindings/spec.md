## ADDED Requirements

### Requirement: The action registry is complete

Every `Action` variant's `key()` id SHALL have an `ActionSpec` (label, hint, default chords, palette flag, search keywords), `Action::from_key(key())` SHALL return the same action, and every declared default chord SHALL dispatch that action through a fresh default keymap. Parametric families (`summon-*`, `custom-action`) are the only sentinels exempt from the spec requirement.

#### Scenario: Variant without a spec

- **WHEN** an `Action` variant is added with a `key()` arm but no `ActionSpec`
- **THEN** `every_action_key_has_a_spec_and_round_trips` fails naming the id

#### Scenario: Spec chord disagrees with the keymap

- **WHEN** a spec declares a default chord the default keymap binds to a different action
- **THEN** `declared_default_chords_actually_dispatch` fails
