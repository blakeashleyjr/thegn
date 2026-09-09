# Tasks — named chat/webhook sinks

## 1. Config and routing

- [x] 1.1 Add explicit named sinks, legacy one-sink compatibility,
      `push`/`push:<name>` targeting, and per-sink priority floors.
- [x] 1.2 Validate unique names, implemented/reserved kinds, and SecretRef-only
      webhook endpoints with pure unit tests.

## 2. Providers and runtime

- [x] 2.1 Implement and unit-test stable generic webhook, bounded Discord, and
      bounded Slack payloads, including Discord mention suppression.
- [x] 2.2 Implement per-sink token buckets, bounded retries/`Retry-After`, and
      drop accounting.
- [x] 2.3 Fan out routed notifications to bounded per-sink workers without
      blocking the producer/event loop; retain ntfy behavior.
- [x] 2.4 Add offline, redacted provider probes that never POST.

## 3. Documentation and reconciliation

- [x] 3.1 Document sink configuration, routing, rate limits, and the outbound
      data boundary in example config and notification help.
- [x] 3.2 Reconcile this change with delivered scope and validate it with
      `openspec validate add-chat-webhook-sinks --strict`.
