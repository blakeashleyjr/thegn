# Inbox

## ADDED Requirements

### Requirement: Daemon-only polling

thegn SHALL poll inbox accounts only in the daemon, and only when `[daemon]
enabled`, `headless_producers` and `[inbox] enabled` are on and an enabled
account of an implemented kind exists. Otherwise it SHALL start no task, timer
or lease.

#### Scenario: Inert

- **WHEN** `[inbox]` is absent or disabled
- **THEN** no inbox task, lease row or process exists

### Requirement: Baseline first poll

The first successful poll of an account, or the first after its configuration
digest changes, SHALL record the seen-set, notify at most 20 items and submit no
rule events.

#### Scenario: Old mail

- **WHEN** an account with 500 existing items is first polled
- **THEN** no `inbox_item` rule fires

### Requirement: Untrusted items and secret custody

Every item field SHALL be sanitized and length-capped, and item text SHALL never
become a command, environment value, path, scope, rule, autonomy level or
approval. Credentials SHALL be secret references resolved in the daemon, and a
literal `token` or `password` SHALL fail validation.

#### Scenario: Literal token

- **WHEN** an account sets `token = "abc"`
- **THEN** `config validate` refuses it
