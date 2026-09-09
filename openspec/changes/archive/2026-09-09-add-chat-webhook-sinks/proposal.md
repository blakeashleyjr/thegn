# Add named outbound chat and webhook sinks

Linear: THE-62

## Why

Outbound chat delivery is the useful, bounded integration: it reuses the
notification router and push-provider worker without embedding an always-on
bot framework or inventing another command-admission policy.

## Delivered design

- `webhook`, `discord`, and `slack` are implemented beside `ntfy`; `telegram`,
  `gotify`, and `pushover` remain explicitly reserved.
- `[[notifications.push.sinks]]` provides ordered named sinks. Routing can
  target every configured sink with `push` or one sink with `push:<name>`, and
  each sink can impose a minimum effective priority.
- Generic webhook payloads are versioned; Discord and Slack payloads apply
  platform bounds and priority presentation. Discord suppresses automatic
  mention parsing.
- Webhook URLs are bearer credentials and therefore accept only `env:` or
  `file:` SecretRefs. Diagnostics and errors identify the sink, never its URL.
- One bounded worker per sink performs off-loop delivery. Token buckets,
  bounded retries, and bounded `Retry-After` handling prevent a chat outage or
  burst from blocking notification producers or creating an unbounded queue.
- Configuration/help documentation explains routing and the data-egress
  boundary. Provider probes validate resolved configuration and request shape
  without sending a visible message.

## Non-goals

- In-process Discord/Slack/Telegram bots or inbound chat commands.
- Rich templating, threads, interactive components, or bundled chat clients.
- A second notification channel outside the existing push-provider seam.

## Evidence

Delivered across the named-sink routing/config commits and the webhook,
Discord, Slack, rate-limit, worker, and documentation commits from
`6eef839c` through `eef839c6`.
