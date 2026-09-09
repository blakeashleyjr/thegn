# Notifications

## ADDED Requirements

### Requirement: Named push sinks share notification routing

The notification configuration SHALL support ordered, uniquely named push
sinks. `push` SHALL select every configured sink, `push:<name>` SHALL select
only that sink, and an optional per-sink minimum priority SHALL be applied to
the effective routed priority. The legacy scalar ntfy configuration SHALL
continue to behave as one sink.

#### Scenario: A rule targets one sink

- **WHEN** a rule routes a matching notification to `push:oncall`
- **THEN** only the configured `oncall` sink receives it

#### Scenario: Sink floors apply independently

- **WHEN** a Notice routes to all sinks and a Slack sink requires Alert
- **THEN** eligible lower-floor sinks receive it and the Slack sink does not

### Requirement: Webhook, Discord, and Slack are implemented sink kinds

The push-provider seam SHALL implement `webhook`, `discord`, and `slack`
beside `ntfy`. Generic webhook payloads SHALL use the stable versioned fields
`v`, `kind`, `priority`, `message`, `source`, `worktree`, and `ts`. Discord and
Slack payloads SHALL obey their text bounds and present effective priority;
Discord payloads MUST suppress automatic mention parsing. `telegram`,
`gotify`, and `pushover` SHALL remain reserved until separately implemented.

#### Scenario: A queue alert reaches Slack

- **WHEN** an Alert-priority queue notification routes to a Slack sink
- **THEN** the provider sends a bounded Slack webhook payload with the alert
  presentation while preserving the ordinary inbox record

#### Scenario: Discord text cannot mass-mention

- **WHEN** routed notification text contains `@everyone`
- **THEN** the Discord payload disables mention parsing

### Requirement: Webhook endpoints remain secret

Discord, Slack, and generic webhook endpoints SHALL be configured only through
`env:` or `file:` SecretRefs. Raw URLs MUST fail config validation, and
resolved endpoints MUST NOT appear in logs, error text, or probe output.

#### Scenario: A raw URL is rejected

- **WHEN** a sink declares `url = "https://hooks.example/secret"`
- **THEN** validation names the sink and requires a SecretRef without echoing
  the URL

### Requirement: Sink delivery is bounded and off-loop

Each configured sink SHALL deliver from its own bounded worker with a
provider-appropriate token bucket, bounded transient retries, and bounded
`Retry-After` handling. Queue overflow or a rate-limit wait beyond the retry
budget SHALL drop and count the message rather than block producers or grow an
unbounded backlog.

#### Scenario: A burst cannot wedge notification producers

- **WHEN** a sink receives more messages than its queue/rate budget permits
- **THEN** excess messages are counted as dropped and producer execution
  continues without waiting for network I/O

### Requirement: Provider probes do not send messages

Sink probes SHALL report provider capability, configuration/secret-resolution
state, and offline request-shape readiness without making a network POST.

#### Scenario: Diagnostics are channel-silent

- **WHEN** a configured Discord or Slack provider is probed
- **THEN** no visible test message is delivered and no endpoint is revealed
