# THE-19 — authorized fix for the load-sensitive hook-drain test (Lead work order)

files:

- crates/thegn-host/src/hook_run.rs
- (its test module)

## Authorization

Row 327 delivered the platform-seam move and escalated the last blocker. Its
diagnosis is accepted:

```
background_descendant_cannot_hold_hook_completion_open
  timing assertion failed at 1.424399617s
```

The gate reached 4,718/4,719 passing; this is the only failure.

## The judgement call, stated plainly

This test was introduced by this branch (row 303) to prove the bounded
pipe-drain fix. **The production behaviour is correct and must not change** —
a hook whose background descendant inherits the pipe must not hold completion
open, and the 250 ms drain deadline is the mechanism.

What is wrong is the **test**, which asserts wall-clock timing and therefore
fails whenever the machine is loaded. It failed here because several pipeline
workers were compiling concurrently. A test that passes only on an idle box is
a flaky test: it will fail the same way in CI and on any busy developer machine,
and a flaky gate is worse than no gate because it teaches people to re-run it.

## Done criteria

- `cargo nextest run -p thegn-host -E 'test(background_descendant_cannot_hold_hook_completion_open)'`
  passes reliably, INCLUDING under load. Verify by running it while the box is
  busy, not just once on an idle machine.
- **Do not** simply raise the timeout to a bigger wall-clock number — that
  re-creates the same flake with a larger constant. Prefer, in order:
  1. assert the OBSERVABLE PROPERTY rather than the duration — that completion
     is reported without waiting for the descendant, e.g. by signalling from the
     drain path and asserting ordering/causality;
  2. inject the deadline so the test drives a controlled clock instead of the
     real one;
  3. only if neither is workable, a generous bound with a comment explaining
     why the property could not be asserted directly.
- Do not change the 250 ms production deadline or any drain/kill semantics.
- Then run the full gate: `RUSTC_WRAPPER= THEGN_ALLOW_HEAVY=1 just test`
  (the wrapper-free form — sccache is unreachable in this sandbox, tracked as
  THE-90). Report the full result; this should be the last blocker before
  THE-19 can land.
