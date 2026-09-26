# Primary review + greenlight — THE-159

Reviewing row 568's investigation. **APPROVED to implement.**

The evidence is confirmed on this branch and the cast is exactly as described:
`platform/unix.rs` casts a stored `u32` to `i32`, so `u32::MAX` and anything
above `i32::MAX` wrap negative — and a negative argument to `kill(2)` means
_signal the process group_. Spawn stores only `child.id()`, so there is no
identity to check against.

## Your question — Windows: no parity work in this lane

`platform/windows.rs` being PID-only is **out of scope**. Windows does not have
the negative-PID hazard that makes this urgent (no `kill(-pid)` group
semantics), so the defect this issue names does not exist there. Building
Windows PID-reuse identity is a separate piece of work.

What must hold on Windows: the **range validation** applies everywhere (it is
platform-free), and the Windows path must not silently become the lenient one.
If a shared validation helper is the natural shape, put it in the platform-free
layer and let both call it. Record the missing Windows identity binding as an
explicit follow-up finding.

## Restated decisions

- Reject **before** converting: zero, above the platform's safe positive range,
  and anything that does not round-trip `u32 -> i32`.
- Never signal a negative PID derived from stored state. Group signalling is
  explicit at the call site, for a group this supervisor created — express that
  in the type, not via a sign bit.
- Prove identity before TERM/KILL by binding the PID to process start time
  (Linux: `/proc/<pid>/stat` field 22). **Fail closed** — a process you cannot
  identify is one you do not signal.
- A vanished process and a stale file are normal: clean up quietly, do not
  surface them as errors.

## Tests

`u32::MAX`, a value that casts negative, `0`, PID reuse (same PID, different
start time), malformed/empty state, and a vanished process. In every refusal
case assert **no signal was sent** — not merely that an error was returned. A
test that only checks the return value would pass against the current bug.

## Scope

`crates/thegn-host/src/model_proxy_daemon.rs` and
`crates/thegn-host/src/platform/unix.rs`. Platform code stays behind
`platform/`. Do not touch THE-160's listener-identity work.
