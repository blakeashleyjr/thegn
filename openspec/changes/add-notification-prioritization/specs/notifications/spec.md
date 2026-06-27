# Notifications

## ADDED Requirements

### Requirement: Notification priority is derived from kind with overrides

Each notification's priority (Alert / Notice / Info) SHALL be derived from its `NotificationKind` at read time (no stored priority column) and MUST be overridable per kind via `[notifications.priority]`, so a config remap reclassifies even historical rows live.

#### Scenario: Default classification

- **WHEN** priorities are computed with no config override
- **THEN** the four failure kinds are Alert, lifecycle kinds (worktree created,
  process exited) are Info, and the rest are Notice

#### Scenario: Config remap reclassifies live

- **WHEN** `[notifications.priority]` demotes a kind
- **THEN** existing rows of that kind are reclassified without a migration

### Requirement: Priority coherently drives flag, count, and toast

The red attention flag SHALL count only unread Alert notifications, the neutral unread count SHALL be unread Alert+Notice, Info MUST never increment any counter (but still appears in the inbox), and toast urgency MUST follow priority (Alert→Critical, Notice→Normal, Info→Low).

#### Scenario: Info-only inbox raises no flag

- **WHEN** the only unread notifications are Info (e.g. worktree created)
- **THEN** the panel header shows a neutral state with no red attention flag

#### Scenario: A failure raises the flag

- **WHEN** a TestFailed notification is unread
- **THEN** the header shows the red attention flag and a Critical desktop toast is
  eligible
