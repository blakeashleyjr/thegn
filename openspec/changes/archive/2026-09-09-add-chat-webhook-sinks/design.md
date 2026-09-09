# Design — named outbound notification sinks

## One router and one worker model

Chat transports are push providers. A route decision carries an ordered set of
sink names; `push` expands to every configured sink and `push:<name>` selects
one. Sink priority floors are applied after notification rules compute the
effective priority. The legacy scalar ntfy form remains a one-sink
configuration, while explicit sinks require unique names.

The host fans a routed notification out to one bounded worker per sink. Each
worker owns its limiter and delivery accounting. Full queues and messages that
cannot fit the bounded rate-limit/retry window are dropped and counted rather
than applying backpressure to the producer. HTTP 429 responses honor a numeric
`Retry-After`, clamped to the provider's retry budget.

## Payloads

- `webhook`: stable JSON `{v, kind, priority, message, source, worktree, ts}`.
- `discord`: bounded `content`, title/priority-color embed, and
  `allowed_mentions.parse = []` so routed issue/log text cannot broadcast.
- `slack`: bounded fallback `text`, a mrkdwn section, and priority color.

Payload shaping is pure and tested separately from HTTP. Provider delivery is
time-bounded and retries only transient/rate-limited failures.

## Security and diagnostics

Discord, Slack, and generic webhook URLs are bearer credentials. Static config
validation accepts `env:`/`file:` SecretRefs and rejects raw URLs. Resolution,
logs, and probe reports redact endpoint material. Message content intentionally
leaves the machine only after normal routing, priority, DND, mode, and profile
policy selects that sink.

Provider probes validate configuration, secret resolution, capabilities, and
offline request-shape readiness; they never POST. Delivery counters belong to
runtime worker telemetry, not to a promised doctor output contract.

Interactive bots remain external control-plane clients. Thegn does not add a
gateway connection, bot-token credential, inbound listener, or alternate
authorization table here.
