# Automation Rules

## ADDED Requirements

### Requirement: Headless firing

With `[daemon] headless_producers = true`, thegn SHALL refresh every registered
repo's tracker and poll PRs in the daemon, so rules fire with no UI open.
Without it, only the focused repo is refreshed while a UI is open.

#### Scenario: No window

- **WHEN** an enabled `issue_matched` rule exists and no UI is open
- **THEN** the daemon's tracker producer evaluates it
