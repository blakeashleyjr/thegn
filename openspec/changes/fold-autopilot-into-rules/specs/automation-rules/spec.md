# Automation Rules

## ADDED Requirements

### Requirement: Autopilot as a rule action

thegn SHALL provide a config-origin-only `autopilot.run` action for
`issue_matched` rules, capped at `review` autonomy, and SHALL synthesize an
`autopilot:<repo>` rule for each repo with `[autopilot]` enabled. A Lead-authored
rule naming the action SHALL be refused.

#### Scenario: Lead authors it

- **WHEN** the Lead writes a rule with `autopilot.run`
- **THEN** the write is refused

### Requirement: Budgets cover autopilot

Autopilot runs and the Lead's manual spawns SHALL count against
`max_concurrent_lanes` and `max_spawns_per_day`.

#### Scenario: Limit reached

- **WHEN** the daily spawn limit is used up
- **THEN** the next autopilot run is skipped as `budget`
