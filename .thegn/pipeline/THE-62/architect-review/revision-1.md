---
files:
  - crates/thegn-core/src/notification_render.rs
  - crates/thegn-host/src/push_notify.rs
  - crates/thegn-host/src/notification_delivery.rs
overlaps: []
after: []
---

# THE-62 revision 1 — enforce the promised delivery policy and event templates

The current implementation is otherwise structurally aligned with the design,
but these are functional gaps, not polish:

1. `crates/thegn-host/src/push_notify.rs` imports no limiter and never creates
   or consults `thegn_svc::push::rate_limit::TokenBucket`. The three providers
   therefore run at unrestricted queue/worker speed; `DeliveryEvent::RateLimitDrop`
   can never occur. Add one limiter per named provider/sink, owned by the
   worker and initialized from the provider kind. Before each publish, apply
   `Decision::{Send, Defer, Drop}` with a bounded retry window. `Send` publishes
   the job; `Defer` waits only on the existing off-loop worker/runtime and then
   retries the same bounded job; `Drop` increments that sink's
   `RateLimitDrop` counter and does not publish. Preserve `try_send` queue
   semantics, independent sink state, bounded provider attempts, and the
   durable inbox behavior. Do not add an event-loop timer or an unbounded
   deferred queue.

   Add a deterministic worker-side policy helper/test (or equivalent focused
   test) proving that a configured sink actually consumes tokens, defers within
   the budget, and drops outside it. The existing pure `rate_limit` tests alone
   are insufficient because the production worker currently bypasses them.

2. `crates/thegn-core/src/notification_render.rs` currently renders every
   catalog kind with one generic `kind.label(): message` shape. The design
   requires a substrate-free built-in template table covering every
   `NotificationKind::ALL` entry. Add an exhaustive per-kind template mapping
   (with stable generic fallback only if explicitly represented as a table
   entry), keep the caller-supplied notification data as the only input, and
   retain the existing flavor escaping, Unicode truncation, and secret-free
   boundary. Do not add user-configurable templates or runtime/I/O access.
   Tests must prove the table is exhaustive and exercise representative
   event-specific output, all flavors, stable generic fields, and the existing
   Discord bound.

The architect has already applied and committed small adjacent corrections on
the branch: inert default sinks no longer appear in route decisions, rendered
payloads retain the source reference, delivery snapshots pulse the existing
terminal waker, and ntfy transport diagnostics are URL-redacted. Preserve
those corrections while implementing this chunk.
