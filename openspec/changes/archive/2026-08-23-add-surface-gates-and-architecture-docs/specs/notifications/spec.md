## ADDED Requirements

### Requirement: The kind list has one source

`NotificationKind::ALL` SHALL be the only list of built-in notification kinds: `thegn notify push --help` MUST generate its kind list (with default priorities) from it, and `config.toml.example`'s `[notifications.priority]` prose MUST name every kind, pinned by a test.

#### Scenario: New kind

- **WHEN** a kind is added to the enum but not to the example prose
- **THEN** `example_config_prose_names_every_kind` fails naming it
