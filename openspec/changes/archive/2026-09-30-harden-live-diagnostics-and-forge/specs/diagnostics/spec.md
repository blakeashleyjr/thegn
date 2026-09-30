## ADDED Requirements

### Requirement: Plain diagnostic fields obey the selected color policy

The diagnostic formatter SHALL suppress formatter-generated ANSI styling in
structured event fields when plain output is selected, including file logs and
redirected CLI stderr. The field formatter SHALL obey that policy independently
of the outer layer color default or ambient NO_COLOR environment variable.

#### Scenario: Plain file and redirected stderr contain structured fields

- **WHEN** a plain timestamped file event or plain CLI stderr event contains a message and structured fields
- **THEN** the rendered fields SHALL retain their values without formatter-generated ANSI escapes

#### Scenario: Explicit terminal color remains enabled

- **WHEN** colored terminal output is selected
- **THEN** the formatter SHALL preserve the existing branded color output

#### Scenario: JSON fields remain unstyled

- **WHEN** a JSON event is emitted through a color-capable outer layer
- **THEN** it SHALL remain valid JSON without formatter-generated field styling
