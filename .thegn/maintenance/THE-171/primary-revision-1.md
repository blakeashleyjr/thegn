# Primary review — THE-171 revision 1 (reviewing row 627, commit b0dd24e8)

Good, honest work, and the report is the kind that makes review possible: you
listed four residual gaps instead of claiming completeness. Three of the four are
accepted below. **One is a required fix**, and it happens to be in the exact spot
the issue exists to protect.

## REQUIRED — the KILL must be liveness-verified, not unconditional

You wrote: _"Unix KILL is unconditional after the 300ms grace instead of
liveness-verified."_

That has to change. The greenlight said "verify liveness through the owned
identity and force-kill survivors", and the reason is the whole point of THE-171:
an unconditional `SIGKILL` to a numeric PGID after a grace period is a signal to
whatever holds that group **now**. The grace window is precisely when the group
can exit and the ID become reusable — the wait is what creates the hazard, so
adding a wait without a re-check moves the race rather than closing it.

Required shape:

1. After the grace expires, **re-verify through the owner** that the group we
   still hold is the group we spawned, and that it is still alive.
2. **Only then** signal, and signal through the same owned identity — never a
   number re-read from anywhere.
3. If verification fails or is inconclusive, **do not signal.** Record it. A
   leaked process we can name is strictly better than a killed process we cannot.

If the platform cannot express "verify and signal" without a window between the
two, say so explicitly in a comment stating what remains open rather than
implying a guarantee. That honesty is what the rest of this module does.

## Accepted as written — do not change these

- **The pane-drop reader is not joined on the UI thread.** Your reason is right: a
  bounded shared-channel send can deadlock the event loop, and this repo's first
  invariant is that the loop never blocks. Deviating from "retain join receipts"
  here is correct. **Add a comment at the site saying why**, so the next reader
  does not "fix" it back into a deadlock — that comment is the deliverable.
- **Windows Job Object termination is hard-kill with no graceful window.** Accepted
  as scoped. Document it as a known limitation where a Windows reader will find it.
- **Windows integration coverage outstanding.** Accepted — there is no Windows
  runner in this loop and a fabricated test would be worse than none. State it as
  debt.

## REQUIRED — the missing integration coverage, narrowed

Full PTY-actor Kill/Exit and fd-cleanup integration coverage is a real gap, but I
am not asking for all of it. Add **one** test that pins the race this issue is
about:

> A Kill arriving concurrently with a natural Exit resolves through the owner
> exactly once, and the child is reaped exactly once.

That is the property an `AtomicBool` could not provide. If it is already covered
by `pty_owner_tests::concurrent_teardown_reaps_only_the_owned_child_group`, say so
and skip it — do not write a second test for the same property.

**A test here must never be able to signal a process outside its own fixture.**
Spawn what you signal; assert identity before signalling. A flake in this area
kills a developer's editor.

## Scope

Nothing beyond the above. Do not extend to new platforms, do not refactor the
daemon's teardown further, and do not touch `git`/gate/queue code.

## Validation

Attempt `nix develop --command cargo check -p thegn-host --all-targets` and the
narrow tests you listed. **The pipeline sandbox mounts `/nix/store` read-only, so
this usually fails outright** — say exactly that and stop if it does. The primary
runs clippy, the full workspace nextest and smoke centrally.

Never report a verdict for code you could not compile; state what you could not
run.
