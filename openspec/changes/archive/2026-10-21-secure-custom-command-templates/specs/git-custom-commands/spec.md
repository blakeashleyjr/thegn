## ADDED Requirements

### Requirement: Repository values remain inert command arguments

Custom Git commands SHALL compile into typed argument data or validated shell text before dispatch. Argument templates SHALL keep each value in its original argument. Shell templates with placeholders SHALL require explicit safe policy admission and SHALL reject unsupported or ambiguous contexts before execution. Missing values, invalid paths/filters and NUL SHALL fail closed.

#### Scenario: Hostile commit subject

- **WHEN** a selected subject contains quotes, newlines, backticks, redirections or command substitutions
- **THEN** safe expansion passes it as one argument and executes no embedded shell syntax

#### Scenario: Legacy or quoted placeholder

- **WHEN** a shell template omits its migration policy or embeds a placeholder in a quote, assignment or unsupported shell context
- **THEN** validation identifies the command field and invocation refuses without discarding unrelated configuration

### Requirement: Execution preserves template admission

Popup, discarded-output and terminal modes SHALL consume the same admitted command type. Local, SSH and managed-provider projections SHALL preserve argument boundaries. Diagnostics SHALL omit expanded values. Raw expansion SHALL require both a conspicuous unsafe policy and an explicit dangerous filter.

#### Scenario: Raw expansion without policy

- **WHEN** a dangerous raw filter is used under safe policy
- **THEN** compilation refuses before any process is started
