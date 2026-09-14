## ADDED Requirements

### Requirement: Keyboard diagnostics omit input and action payloads

Keyboard diagnostics SHALL omit character values and arbitrary action payloads
under all supported log levels. Diagnostics MAY retain safe key classes,
modifier bits, and match outcomes without changing dispatched or forwarded input.

#### Scenario: Printable input under global debug or trace logging

- **WHEN** a character is forwarded to a child while broad diagnostics are enabled
- **THEN** neither the raw nor normalized character appears in the diagnostic

#### Scenario: A configured custom action matches

- **WHEN** an input event resolves to an action carrying a custom payload
- **THEN** the log reports a match without serializing the action payload
