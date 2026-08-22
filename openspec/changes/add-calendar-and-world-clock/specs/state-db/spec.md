# State DB

## ADDED Requirements

### Requirement: Calendar events are cached in the state database

The state database SHALL carry a `calendar_events` table holding one row per
event per account, and a `calendar_sync` table holding each account's provider
cursor, last fetch time, last error, and synced horizon. Both SHALL be added by
an additive migration with a `user_version` bump, so an existing database
upgrades in place without losing its other data.

One row per event, rather than one document per account, is required so that an
incremental sync can apply a single deletion without refetching the account, and
so a one-month query does not deserialize a year.

Both tables are caches — the provider is the source of truth — so dropping them
MUST be safe and MUST result only in a re-sync.

#### Scenario: An older database gains the tables on open

- **WHEN** a database created before the calendar existed is opened
- **THEN** both tables are created, `user_version` is advanced, and every
  pre-existing row in other tables is preserved

#### Scenario: Events survive a restart

- **WHEN** events are synced and thegn is restarted
- **THEN** the calendar shows them without waiting for a new fetch

### Requirement: A range query includes recurrence masters

A query for events in a date window SHALL return every recurring event
regardless of its own start and end, in addition to non-recurring events
overlapping the window. A recurrence master's own span is unrelated to when its
occurrences fall, so filtering it by that span would silently hide a repeating
event from every month after the first.

#### Scenario: A recurring event defined years earlier

- **WHEN** a weekly event whose first occurrence was years ago is queried for the
  current month
- **THEN** the event is returned so it can be expanded

#### Scenario: A finished one-off event

- **WHEN** a non-recurring event that ended before the window is queried
- **THEN** it is not returned

### Requirement: A failed sync never damages the cache

Recording a sync failure SHALL leave the account's cached events and its
provider cursor unchanged, so a transient failure degrades to stale data rather
than to no data, and the next attempt can still resume incrementally. A
subsequent success SHALL clear the recorded error.

A full fetch returning no events SHALL be applied only when the account has
nothing cached. When the account does have cached events, the empty result MUST
be treated as suspect and recorded rather than applied, because a provider or
proxy returning an empty body would otherwise erase the calendar with no error
to explain it.

#### Scenario: A transient fetch failure

- **WHEN** an account's fetch fails
- **THEN** its cached events and cursor are unchanged and the error is recorded

#### Scenario: An empty result against a populated cache

- **WHEN** a full fetch returns no events for an account that has cached events
- **THEN** the cached events are kept and the anomaly is recorded

#### Scenario: An empty result against an empty cache

- **WHEN** a full fetch returns no events for an account with nothing cached
- **THEN** the result is accepted and no error is recorded

### Requirement: The event cache is bounded

Cached events SHALL be pruned on a growth bound so a long-lived install does not
accumulate history indefinitely. Pruning MUST NOT remove recurrence masters,
whose old start dates still generate current occurrences.

#### Scenario: Pruning old events

- **WHEN** the prune runs
- **THEN** non-recurring events that ended long ago are removed and recurring
  events are kept
