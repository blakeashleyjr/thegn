# THE-35 architect revision 1 — complete hydration producer funnel

## Findings

1. `crates/thegn-host/src/hydrate.rs:3715` writes the catalog kind
   `mentioned` directly with `put_notification_once`. When GitHub mention
   polling discovers a new mention, it therefore bypasses `NotifyState` and
   cannot apply `notifications.sound.per_kind.mentioned`, route/DND/focus
   gates, or the configured sound fallback.

2. `crates/thegn-host/src/hydrate_tracker.rs:117` does the same for the catalog
   kind `overdue`. A user may configure `per_kind.overdue`, but the re-derived
   tracker event never reaches the route, and the required emit-once behavior
   is not represented in the sound path.

## Required fix

Add a host notification helper for the `put_notification_once` producer shape
(for example `notify::record_once`) that:

- computes the normal core decision;
- preserves `(kind, source_ref, message)` emit-once semantics atomically through
  `put_notification_once`;
- emits sound/toast/push only when a new row was inserted and the decision
  authorizes that channel; and
- keeps the DB operation on the existing hydration worker, never the loop.

Migrate both call sites above to that helper, retaining their existing
durable-only fallback when no live `NotifyState` exists. Add focused host tests
that prove a configured `per_kind` sound is emitted for the first mention and
overdue observation but not on the second identical hydration pass, while a
drop/DND/focus rule still suppresses it.

Do not route the separate daemon/CLI writers or the intentionally durable-only
`disk_cleaned` bookkeeping kind; those have no compositor `NotifyState` or are
not members of `NotificationKind::ALL`.
