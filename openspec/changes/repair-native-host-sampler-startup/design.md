# Design

## Worker sequencing

Keep the worker thread and the existing stop/park atomics. After its QoS start
hook, the worker checks `stop` and `parked` before creating `StatsSampler`.
Create the sampler once on the worker after observing active state, then reuse
it. This keeps blocking collector initialization off the UI and Tokio threads
and lets an initially hidden source park before platform discovery begins.

On activation after a park, keep the existing discarded sample and short
priming delay before publishing again. Sampling and hardware probes remain on
the worker; shared ring retention and queries are unchanged. `Drop` still sets
stop, unparks the worker, and delegates joining to the reaper.

## Test seam

Inject the sampler factory at the private worker constructor. Unit fixtures
start parked and use a lightweight counter snapshot producer, so lifecycle
assertions do not depend on host hardware, sysinfo enumeration, Apple HID
discovery, subprocess timing, or contention between parallel tests. An explicit
assertion proves the factory is untouched while parked and invoked after
activation.

## Architecture impact

No event-loop wake path or render damage channel changes. Existing `set_active`
activation and worker `unpark` behavior remain. No SQLite or config changes.
