# System monitor

## ADDED Requirements

### Requirement: A zero fleet burn hides its masthead meter

The `fleet` masthead widget SHALL be hidden while its hourly burn is zero, when
no listed VM is unpriced and no warning applies. A zero burn with an unpriced VM
or a warning MUST still render. `[bars] fleet_always_show = true` MUST keep the
zero meter on the bar whenever VMs are listed. Nothing listed and nothing
burning stays absent in both modes.

#### Scenario: Stopped VMs by default

- **WHEN** VMs are listed, all stopped, and `fleet_always_show` is false
- **THEN** the `fleet` widget does not render at any width

#### Scenario: Zero meter kept on request

- **WHEN** VMs are listed, all stopped, and `fleet_always_show` is true
- **THEN** the `fleet` widget renders `$0.00/h`

#### Scenario: Unpriced VM keeps a zero burn visible

- **WHEN** the burn is zero but one listed VM has no known price
- **THEN** the `fleet` widget renders `$0.00/h +1`
