## ADDED Requirements

### Requirement: Numeric duration policy has unit-aware supported bounds

Configuration schema and strict validation SHALL share supported maxima for
numeric durations: 31 days for periodic cadences and ten 365-day years for other
durations, expressed in each field's documented unit. Existing feature-specific
floors and zero/None disable or inheritance semantics SHALL remain intact.
Epoch timestamps MUST NOT be interpreted as durations.

#### Scenario: An explicit duration override exceeds its supported range

- **WHEN** a CLI, environment, profile, or authored config change introduces an out-of-range duration
- **THEN** its explicit layer or write is rejected before publication
- **AND** an existing config file retains its prior bytes on write rejection

#### Scenario: A permissive base file contains an invalid duration and security policy

- **WHEN** the base file successfully parses but one duration is outside its supported range
- **THEN** the runtime retains the parsed security settings and diagnoses the duration
- **AND** safe consumer conversion or destructive-policy quarantine handles that value without whole-file default fallback

### Requirement: Shared refresh cadence conversion cannot corrupt unrelated schedules

Every enabled periodic cadence SHALL resolve through a checked, nonzero slot
conversion without intermediate overflow. Nonintegral slot boundaries SHALL
round up. Disabled schedules SHALL remain explicit and MUST NOT use zero as a
modulo divisor. A shared ticker panic SHALL emit a worker-failure error and an
existing terminal wake without introducing idle polling.

#### Scenario: Adversarial cadence bypasses strict validation

- **WHEN** programmatic input supplies a cadence of 2^61 seconds or u64::MAX
- **THEN** runtime conversion produces a bounded nonzero schedule with identical debug and release semantics
- **AND** unrelated periodic schedules retain their own due decisions

#### Scenario: The shared refresh worker unwinds

- **WHEN** the worker panics
- **THEN** it publishes one explicit failure diagnostic and pulses the existing waker
- **AND** ordinary worker shutdown emits neither a failure diagnostic nor an extra wake
