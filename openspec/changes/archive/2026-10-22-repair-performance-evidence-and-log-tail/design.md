# Accurate, bounded instrumentation

Legacy render/flush/input rollup fields remain available and are explicitly
submission-side measurements. Add metric version, elapsed interval and sample
counts. Writer metrics distinguish queue wait, sink write duration and successful
completion. Sink completion is not terminal paint or application-response
acknowledgement. Failed writes and out-of-band bytes are not successful frames.
Writer aggregates remain bounded, add no wake source, and are drained without
waiting on an in-progress sink write.

Preserve the earliest observed pending input across preemption and dispatch.
Account active loop segments across early continues and exclude only poll wait;
the event-loop idle ratio is wall time, never process CPU. Attribute hydration
parent/child CPU separately without nesting a charge on the same thread. Mark
full-screen resync independently of the composition category. No timer, cache,
or compositor optimization is justified solely by the uncontrolled live audit.

Log following uses file identity and logical read position. Initial history is
skipped, replacement files begin at zero, detected truncation resets the cursor,
and partial records do not cross file generations. A bounded retired-file drain
prevents an old writer from starving its replacement. Missing files are retried.
Copytruncate followed by regrowth beyond the old offset between observations
cannot always be distinguished from append; this limitation remains explicit.
