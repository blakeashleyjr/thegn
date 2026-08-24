# Notifications

## ADDED Requirements

### Requirement: Chat and generic webhook sinks are implemented push kinds

The push-provider seam SHALL implement `webhook`, `discord`, and `slack` sink
kinds beside `ntfy`: `webhook` POSTs a stable versioned JSON payload
(`{v, kind, priority, message, source, worktree, ts}`); `discord` posts a
Discord incoming-webhook payload with the message truncated to the platform's
2000-character bound and the effective priority mapped to a color; `slack`
posts a Slack incoming-webhook payload with priority mapped to color. Payload
shaping MUST be a pure function under unit test; delivery inherits the seam's
best-effort off-loop contract (bounded worker, bounded retries,
drop-on-overflow, never blocking the event loop). `telegram`, `gotify`, and
`pushover` remain reserved.

#### Scenario: A queue alert lands in Slack

- **WHEN** a `slack` sink is configured and an Alert-priority
  `queue_needs_human` notification routes to it
- **THEN** the Slack webhook receives one message carrying the notification
  text with the alert color, posted off-loop, and the inbox row is recorded
  exactly as before

#### Scenario: Long messages respect platform bounds

- **WHEN** a notification message exceeds Discord's 2000-character limit
- **THEN** the `discord` payload is truncated with a visible marker and the
  POST is well-formed

### Requirement: Push sinks are named and individually routable

The router SHALL support one or more named push sinks, each with a unique
name and a kind; rules' `channels` selectors SHALL address all sinks
(`push`) or a single sink by name (`push:<name>`), and each sink MAY declare
a `min_priority` floor applied after the effective priority is computed. A
configuration with a single sink table remains valid and behaves as one sink
named by its kind. A `channels` selector naming an unconfigured sink MUST be
a config-load error.

#### Scenario: Alerts to chat, everything to phone

- **WHEN** sinks `oncall` (slack, `min_priority = "alert"`) and `phone`
  (ntfy) are configured and a Notice-priority notification routes to `push`
- **THEN** the phone sink delivers it and the `oncall` sink does not

#### Scenario: A rule targets one sink

- **WHEN** a rule sets `channels = ["push:oncall"]`
- **THEN** matching notifications deliver to the `oncall` sink only

### Requirement: Webhook URLs are secrets

Every sink URL (Discord, Slack, generic webhook) SHALL be configured as a
SecretRef (`env:` / `file:`) — a raw URL literal in config MUST be rejected
at validation naming the sink — and resolved URLs MUST NOT appear in logs,
doctor output, or error messages, which refer to sinks by name.

#### Scenario: Raw URL refused

- **WHEN** a sink configures `url = "https://hooks.slack.com/services/…"`
- **THEN** config validation fails naming the sink and the SecretRef forms

#### Scenario: Errors name the sink, not the secret

- **WHEN** delivery to a sink fails
- **THEN** the logged error carries the sink name and status, never the
  resolved URL

### Requirement: Chat sinks are client-side rate limited

Each sink SHALL enforce a client-side rate limit appropriate to its platform
(per-sink token bucket), honor `429 Retry-After` within the seam's bounded
retry budget, and drop over-limit messages with a counter rather than
queueing unboundedly; drop counters SHALL be visible in the sink's doctor
row. Doctor probes MUST validate configuration and secret resolution without
posting to the sink; sending a visible test message is only ever an explicit
user action.

#### Scenario: A notification burst does not spam or queue

- **WHEN** a burst of routed notifications exceeds a sink's rate limit
- **THEN** messages beyond the bucket are dropped and counted, the worker's
  queue stays bounded, and the event loop is never blocked

#### Scenario: Doctor stays silent in the channel

- **WHEN** `thegn doctor` probes a configured `discord` sink
- **THEN** the probe reports config and secret resolution without any POST
  to the webhook
