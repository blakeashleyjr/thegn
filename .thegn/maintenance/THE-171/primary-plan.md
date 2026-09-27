# Primary review + greenlight — THE-171

Reviewing row 625. **APPROVED, and your safe reading of the persistence question
is the correct one — it is now the decision, not a proposal.**

Your source evidence is the most precise of this batch, and the ordering you
established is the whole bug: `pane_pty.rs` waits and _then_ stores `reaped`,
while `pane.rs` atomic-checks and _then_ signals a numeric PGID. An `AtomicBool`
cannot preserve OS process identity across that window.

## DECISION — persistence is diagnostics-only. Never signal from a persisted PID.

You asked whether "persist enough state for crash recovery diagnostics" could
require crash-time signaling of descendants. **It does not, and it must not.**

Signaling a numeric PID recorded by a daemon that has since died is the
reused-PID hazard this issue exists to remove, in its worst form: the longer the
gap, the likelier the PID belongs to something else. A crash-recovery path that
kills "leftovers" from a stale record could kill an unrelated process on a busy
machine. So: persist daemon generation, session id, identity/phase and shutdown
result **for diagnostics**, mark and retire records as teardown completes, and
never derive a signal target from them. You identified the conflict correctly and
resolved it the right way.

## Confirmed as written

- **One shared per-child lifecycle owner, created with the child before its
  reaper starts**, owning the `Child` plus the platform process-tree handle and
  arbitrating natural-exit observation against shutdown signaling. Both the
  ordinary `PtyPane::drop` path and daemon actor teardown go through it — the
  same owner, not two copies of the idea.
- On Unix, **never let an uncoordinated `wait()` release the group leader
  identity while a signal can still be issued.**
- Explicit shutdown coordinator: stop admitting sessions, stop accepting, snapshot
  and signal every owned session, await under one bounded grace policy, then
  verify liveness through the owned identity and force-kill survivors.
- Deterministic teardown order, and Kill/Exit races idempotent through the owner.
  Retain join receipts rather than detaching tasks.

## Platform and test constraints

Windows Job Object behaviour stays in `src/platform/windows.rs` with the shared
API in `src/platform/` — there is a **shrink-only ratchet** on platform `#[cfg]`
outside `platform/`, and `platform/unix.rs` is already an entry on it, so a new
`#[cfg]` at a call site will fail the build.

**A test here must never be able to signal a process outside its own fixture.**
That is not a style preference: a flaky test in this area kills a developer's
editor. Spawn what you signal, and assert the target's identity before signalling
it.

## Validation

Attempt `nix develop --command cargo check -p <crate> --all-targets` and a narrow
`cargo nextest run -p <crate> <filter>`. **The pipeline sandbox mounts
`/nix/store` read-only, so this usually fails outright** — say exactly that and
stop if it does. The primary runs clippy, the full workspace nextest and smoke
centrally.

Never report `implementation-ready` for code you could not compile; state what you
could not run. Every lane in the previous chain shipped something that did not
build and the primary caught each — that division of labour is expected, an
optimistic report is not.
