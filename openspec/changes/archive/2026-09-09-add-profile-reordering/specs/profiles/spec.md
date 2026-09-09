# Profiles

## ADDED Requirements

### Requirement: Profiles have a stable, user-editable switcher order

The profile switcher SHALL render profiles in a user-controlled order persisted in
shared, never-rerooted config (read from the real `XDG_CONFIG_HOME`, not any
per-profile state root), and the user MUST be able to reorder the highlighted
profile from within the switcher; profiles absent from the stored order SHALL be
appended in a stable, deterministic order rather than reordering the known ones.

#### Scenario: Switcher renders the stored order

- **WHEN** the profile switcher opens and a shared profile order is present
- **THEN** profiles appear in that order, with the active profile still marked,
  and any profile not in the stored order is appended deterministically after it

#### Scenario: Reordering persists across profiles and restarts

- **WHEN** the user moves the highlighted profile up or down in the switcher
- **THEN** the entire new order is written to shared config and the next switcher
  open — in this or any other profile's process — reflects it

#### Scenario: Order state is not per-profile

- **WHEN** the order is read or written
- **THEN** it resolves under the real `XDG_CONFIG_HOME` (never a rerooted
  per-profile state root), so every profile's process observes one shared order
