# Agent Orchestration

## ADDED Requirements

### Requirement: Merge autonomy needs a sealed gate

A rule or lane at `merge` autonomy SHALL land without an approval only when
`[merge_queue] gate_sandbox = "sealed"` resolves for the repo and the gate is
enabled with a non-empty command and a positive timeout. Otherwise it MUST
raise an approval stating why.

#### Scenario: Key unset

- **WHEN** `gate_sandbox` is `off`
- **THEN** a `merge` land behaves like `review` and stops for approval
