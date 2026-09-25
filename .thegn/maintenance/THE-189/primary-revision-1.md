# Primary revision brief — THE-189 (round 2)

Row 549's adversarial review filed five findings. **All five are accepted.**
This is a strong review; treat it as the spec for this round.

## F-001 (high) — stale reminder raises AND permanently strands the cursor

Accepted, and it is the worst of the five: it is both a user-visible wrong
notification and a **permanent functional break**, because
`ReminderCursor.inflight` is never cleared afterwards, so every later reminder
is suppressed for the rest of the session.

Required:

- The reminder worker must not raise a notification whose schedule generation
  has been replaced. Check at the point of raising, not before the evaluation.
- `ReminderCursor.inflight` must be released on **every** exit path, including
  the dropped-envelope path at `run.rs:11745-11747`. A dropped stale envelope is
  still a completion as far as the cursor is concerned.
- Add a regression that reloads mid-evaluation and asserts (a) no stale
  notification, and (b) a _subsequent_ reminder still fires. Point (b) is the
  one that catches the stranding.

## F-002 (high) — fence at publication, not before the await

Accepted, with the scope set explicitly so you do not over-build:

- **Required:** every scheduled class — weather included, it currently checks
  nothing — re-checks the schedule generation **at the publication/cache-write
  boundary**, immediately before it commits, not before it starts the work. A
  stale result is discarded there. That is what satisfies "old ticker tasks
  cannot deliver stale-generation results."
- **Not required:** hard cancellation of in-flight provider/network calls.
  Aborting a request already in flight is a larger redesign than this lane
  carries. Let it finish and drop its result at the fence. Say so in a comment
  so the next reader knows it is deliberate.
- The check-then-publish window must be closed for each class listed in the
  review (weather, CI, PR, calendar, LOC, usage).

## F-003 (medium) — the untagged-refresh guard regressed; restore it

Accepted, and this is the guard the primary singled out when greenlighting the
plan. Coalescing a scheduled and an untagged request into one boolean lets a
user-forced refresh inherit `scheduled_*_generation` and be discarded by the
fence — which is exactly the failure mode the plan's own point 4 promised to
avoid.

Required: **untagged wins.** If any untagged (user-forced or event-driven)
request is coalesced into a class's pending work, the combined job carries **no**
generation and is never fenced. Apply this to every class the review names (PR,
CI, calendar, reminders, LOC, usage, weather). Add a direct regression: a
scheduled and an untagged request for the same class arrive in one drain, a
reload happens, and the untagged work still lands.

## F-004 (medium) — bound the replacement command queue

Accepted. Replace the unbounded `mpsc` command channel with a **latest-value
slot** (the worker only ever honours the newest `Replace` anyway — the drain
already discards the rest semantically, it just allocates them first). A
`Mutex<Option<Replace>>` plus the existing wakeup, or a bounded channel that
overwrites, are both fine. State which you chose.

## F-005 (medium) — tests must drive the real owner

Accepted. The existing tests exercise a local `FakeSlot` and an `AtomicU64`;
they do not prove the production path works. Required coverage must drive the
real `ScheduleOwner`, the replacement command boundary, and the event-loop
envelope, for all seven ticker classes — plus the three new regressions named
above (F-001 stranding, F-003 untagged-wins, and a stale-result-discarded-at-
publish case for F-002). Keep the two tests this review already added.

## Unchanged constraints

Sibling module only — nothing added to `run.rs` beyond wiring. 0% idle: no new
wake source at rest. Pulse the `TerminalWaker` on every send. No blocking I/O on
the loop. `render_plan::plan` stays pure. Fake time, not sleeps. No
cargo/nextest/clippy — the primary runs the gate.

If any of F-001..F-005 turns out to require a change the primary has forbidden
(loop work, a config key, a provider-seam change), STOP and report it rather
than working around it.
