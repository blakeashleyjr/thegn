## ADDED Requirements

### Requirement: Static CLI output bypasses configured startup

The host SHALL classify parsed commands by borrowed exhaustive intent before startup migration and profile reroot. Config path/schema, API list/coverage/schema, and shell completion registration SHALL print through their existing static sources without loading or installing effective configuration, opening a controller, or creating startup state. All other commands SHALL retain their existing configured route in this change.

#### Scenario: Unread configuration does not block static output

- **WHEN** an existing static command is invoked with a missing, malformed or unread FIFO config path and invalid config override text
- **THEN** it exits successfully and prints its existing output without evaluating that configuration

#### Scenario: Migration and profile canaries survive static output

- **WHEN** private legacy config/state roots exist and a named profile is selected by flag or environment
- **THEN** static output leaves those roots and bytes unchanged and creates no profile, database, log or migration marker

#### Scenario: Output compatibility survives early dispatch

- **WHEN** config path uses an explicit relative path, completions use the tg executable alias, or stdout closes early
- **THEN** paths retain their spelling, registrations target the invoked alias, and closed stdout remains a successful best-effort output

#### Scenario: Configured siblings do not gain static authority

- **WHEN** config get, API call, automation test, Open with no-launch, Integrate with dry-run, Setup or no command is classified
- **THEN** it does not take the static return and existing configured behavior is preserved
