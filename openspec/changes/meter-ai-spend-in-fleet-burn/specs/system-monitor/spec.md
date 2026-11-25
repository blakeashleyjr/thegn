# System monitor

## ADDED Requirements

### Requirement: The fleet meter includes metered AI spend

The `fleet` masthead widget SHALL show one combined burn: provider VM burn plus
the metered (pay-per-token) AI cost recorded by the model proxy over the
trailing hour. Subscription-covered requests MUST NOT be counted. The
zero-hide rule MUST apply to the combined total. The fleet popup SHALL show AI
burn, today and month to date split per backend/model, and MUST state when a
cost is estimated or a request could not be priced.

#### Scenario: AI spend alone

- **WHEN** no VM burns and the proxy recorded metered cost in the last hour
- **THEN** the `fleet` widget renders the AI burn as `$/h`

#### Scenario: Sum of VM and AI

- **WHEN** VMs burn $0.50/h and AI burns $0.25/h
- **THEN** the `fleet` widget renders `$0.75/h`
