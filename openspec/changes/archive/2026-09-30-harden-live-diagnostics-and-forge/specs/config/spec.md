## ADDED Requirements

### Requirement: Configuration reload preserves observed connectivity

Installing network configuration SHALL preserve connectivity evidence and
recovery history. Changed thresholds SHALL apply without manufacturing network
success or failure. The state visible to hot readers SHALL match the observed
state machine after a policy installation.

#### Scenario: Reload while offline

- **WHEN** the application is offline and identical configuration is reloaded
- **THEN** normal network refreshes remain paused and the scheduled recovery
  probe remains available at its configured cadence

#### Scenario: Reload during a failure streak

- **WHEN** configuration is reloaded after failures below the offline threshold
- **THEN** the accumulated failures remain evidence for the next network result

### Requirement: Runtime diagnostics remain useful across reloads

The application SHALL bound memory used to suppress identical runtime config
warnings. Suppression SHALL distinguish actual diagnostic content and source
identity/content, allowing changed actionable diagnostics to appear. Strict
validation SHALL continue reporting the complete diagnostic set. Config log
reconciliation SHALL preserve default third-party warning suppression and SHALL
NOT override explicit user logging directives.

#### Scenario: Unchanged legacy configuration is loaded repeatedly

- **WHEN** background hydration reloads an unchanged source with legacy keys
- **THEN** repeated warnings from that source are suppressed while retained in
  the bounded recent-diagnostic cache

#### Scenario: Configuration introduces a different warning

- **WHEN** the source content or actionable diagnostic changes
- **THEN** the new warning is emitted rather than hidden by an earlier warning

#### Scenario: Log level reconciles without an explicit environment filter

- **WHEN** the configured file sink changes its level during startup
- **THEN** application records follow that level and the default suppression of
  third-party warnings remains active
