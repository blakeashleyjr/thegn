# LLM proxy

## ADDED Requirements

### Requirement: Budget rolls up through the worktree's zone

When a request's identity resolves to a worktree scope, the proxy SHALL resolve
that worktree's zone from the shared per-profile database and roll budget up
through `scope → zone → global`: the pre-routing check refuses (or downgrades)
when any of those scopes is over its cap or kill-switched, and spend is attributed
to all present scopes. The zone mapping is resolved per request (no push or
periodic sync), so a reassignment takes effect on the next request.

#### Scenario: A member is refused by its zone cap

- **WHEN** a worktree request is under its own cap but its zone `zone:clientA` is
  over the zone cap
- **THEN** the request is refused for the zone scope

#### Scenario: Spend is attributed to scope, zone, and global

- **WHEN** a worktree request in zone `clientA` records spend
- **THEN** the spend is added to the worktree scope, `zone:clientA`, and global

### Requirement: Zone budget caps are synced from config

The system SHALL push each `[zone.<name>.budget]` cap into the proxy's
`zone:<name>` budget scope without disturbing recorded spend, so the per-request
rollup enforces the config-declared caps.

#### Scenario: Syncing sets limits without clobbering spend

- **WHEN** a zone budget cap is synced over a scope that already has spend
- **THEN** the limits are updated and the recorded spend is preserved
