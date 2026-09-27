# Primary review — THE-171 revision 2 (reviewing row 636, commit 4ecc6050)

You implemented revision 1 correctly and your report was honest — you withheld a
verdict because you could not compile, which was the right call. The primary then
compiled and ran it, and found three things. **The first is my error, and it is the
important one: my revision-1 instruction was too strong and it broke the
grandchild cleanup this whole issue exists to provide.**

## Fixed by the primary already — do not redo

`crates/thegn-host/src/daemon/fork.rs` did not compile: `pty` moves into
`SessionActor::new`, and `pty.pid` was then used by the shutdown-refusal
diagnostic below it (`error[E0382]`). The primary captured the pid before the move
(`let pty_pid = pty.pid;`) with a comment noting it is diagnostics-only, per the
persistence decision. That commit is on the branch. `cargo check -p thegn-host
--all-targets` is now clean.

## CORRECTION 1 — a process GROUP we created is not subject to the reused-PID hazard

`concurrent_teardown_reaps_only_the_owned_child_group` fails on
`assert!(!pid_alive(grandchild))` — **the grandchild survives.** The cause is my
revision-1 wording: I said verify liveness through the owned identity and "if
verification fails or is inconclusive, do not signal." Applied to the group path,
that means once the first teardown reaps the group leader, the second finds the
leader gone, declines to signal, and the surviving `sleep` in that group is never
killed.

That is a real regression, and the reasoning behind my instruction does not apply
to a group:

- **A PID can be recycled** the moment its process is reaped. That is why
  `terminate_proxy_pid` must re-verify the start identity before signalling — keep
  that exactly as it is.
- **A PGID cannot be recycled while the group still has members.** So `killpg` on a
  group _we_ created with `setpgid` either reaches our own remaining members, or
  fails with `ESRCH` because the group is empty. There is no third case in which it
  reaches someone else's process. Reaping the leader is the **normal** path, and the
  members left behind are precisely what a process group exists to let us clean up.

So:

1. **Restore unconditional group signalling** for a group created by this code:
   TERM, grace, then KILL, and treat `ESRCH` as "already gone" rather than an
   error. Do not gate it on the leader being alive.
2. **Keep the PID path strictly as revision 1 made it** — identity re-verified
   immediately before the syscall, no signal when verification fails.
3. **Write the distinction as a comment** at the group-signal site: _the PGID
   cannot be reused while any member remains, so signalling the group after the
   leader is reaped is safe; a PID can be reused, so that path re-verifies._ This
   is the subtlest thing in the change and the next reader will otherwise
   "harden" it back into a leak.

## CORRECTION 2 — the race fixture races itself

`natural_exit_racing_kill_is_owned_and_reaped_once` fails at
`assert!(marker.exists(), "fixture reached its natural-exit point")`. The fixture
is `trap '' TERM; (sleep 0.15; touch MARKER) & wait`, and the "natural exit"
thread waits for `MARKER` before tearing down — but the killer thread's KILL
destroys the subshell that would have created it. The two teardowns share one
process group, so the kill path deletes the precondition the exit path waits on.
It cannot pass, and with correction 1 restoring the group kill it will fail
faster.

**Redesign it around the property, not around a real kill-versus-exit race.** The
bug was in the _owner's serialization_, not in the child's behaviour, so:

> Spawn a child that exits on its own **immediately** (no TERM trap, e.g.
> `sh -c 'exit 0'`). Wait until it has exited. Then have two threads call
> `terminate_and_reap` concurrently, and assert the wait count is exactly 1 and
> both calls return without error.

That pins "a Kill concurrent with a natural Exit resolves through the owner
exactly once" — an `AtomicBool` cannot provide it — without depending on a child
surviving a signal. **No marker file and no sleep-based synchronisation.**

Keep `concurrent_teardown_reaps_only_the_owned_child_group` as it is; with
correction 1 it should pass, and it is the test that proves the grandchild dies.

## Unchanged from revision 1

The two accepted limitations stay accepted and stay documented: the pane-drop
reader is deliberately not joined on the UI thread (a bounded shared-channel send
would deadlock the loop), and Windows Job Object termination is hard-kill with
outstanding integration coverage.

**A test here must never be able to signal a process outside its own fixture.** The
existing `assert_eq!(getpgid(pid), pid)` guard before signalling is exactly right
— keep it in the redesigned test if it signals anything at all.

## Validation

Attempt `nix develop --command cargo check -p thegn-host --all-targets` and
`cargo nextest run -p thegn-host pty_owner_tests`. **The pipeline sandbox mounts
`/nix/store` read-only, so this usually fails outright** — say exactly that and
stop if it does. The primary will re-run both regardless.

Never report a verdict for code you could not compile; state what you could not
run. Withholding the verdict last round was correct — do the same again if you
cannot compile.
