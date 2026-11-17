# Capability Catalog

## ADDED Requirements

### Requirement: The Lead capabilities are cataloged with fixed scopes

The catalog SHALL contain `overview.get`, `notifications.list`,
`notifications.ack`, `automations.put`, `automations.rm`, `automations.enable`,
`automations.runs`, `approvals.list`, `approvals.request`, `approvals.resolve`
and `merge.drain`. Reads SHALL require Read scope, writes Write scope,
`approvals.resolve` Admin scope and `merge.drain` MergeAdd scope. All HTTP routes
SHALL be `POST` with a JSON body.

#### Scenario: Resolving an approval needs Admin

- **WHEN** a Write-scoped caller invokes `approvals.resolve`
- **THEN** the call is refused for insufficient scope

### Requirement: Lead-authored rules are limited to two actions

A rule with `origin = lead` SHALL be refused at put time and at evaluation unless
its action is `notify.push` or `sessions.open`. Config rules MUST NOT be
removable or togglable; those calls SHALL answer Conflict.

#### Scenario: A Lead rule asks for merge.add

- **WHEN** `automations.put` carries an action of `merge.add`
- **THEN** the put is refused and nothing is stored
