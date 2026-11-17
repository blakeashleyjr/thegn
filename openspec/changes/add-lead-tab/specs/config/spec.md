# Config

## ADDED Requirements

### Requirement: The `[lead]` section configures the Lead and is stable-clamped

A `[lead]` section SHALL configure the Lead's agent and skill name. On the stable
channel `Feature::Lead` SHALL clamp it to none.

#### Scenario: Stable channel

- **WHEN** `[lead]` is set on the stable channel
- **THEN** the effective config has no Lead
