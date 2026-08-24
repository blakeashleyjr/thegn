## ADDED Requirements

### Requirement: Every palette row is an action

Every command-palette row SHALL be either an `ActionSpec` (dispatched through `Action::from_key`) or a user `[[actions]]` entry; the palette MUST NOT dispatch any row by a string key outside the keymap registry. Navigation and wizard verbs (connect to root, clone and open, new environment, setup wizard) are ordinary actions with no default chord.

#### Scenario: A string-keyed row is rejected

- **WHEN** a palette row is pushed whose key is not an action id or a user action name
- **THEN** `every_palette_key_is_an_action` fails naming the key

#### Scenario: Palette verbs are rebindable

- **WHEN** the user sets `[keybinds] connect-root = "Alt ."`
- **THEN** that chord performs connect-to-root and the palette row shows it
