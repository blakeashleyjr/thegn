# THE-154: resident plugin lifecycle investigation and proposed repair

Status (September 14): the scoped lifecycle repair landed on local main. Its final assembled gate passed 48 plugin regressions, host integration, and scoped clippy; all 48 passed again during native Linux landing validation. Full THE-154 acceptance remains open. The approved native test/CI follow-up passed all eight new Linux regressions alongside all 48 existing plugin tests; native Windows/macOS execution remains outstanding.
This is an existing lifecycle defect, not a plugin feature expansion.

## Historical defects at the original audit baseline (repaired)

`plugin/session.rs` shares ChildStdin behind a mutex and performs blocking
write_all/flush from SessionWriter callers, including compositor callbacks.
Its stdout reader closes stdin then holds the Child mutex across unbounded
wait(); a process that closes stdout but remains alive prevents kill from ever
acquiring the same mutex. ResidentSession has no Drop ownership, reader/stderr
threads are detached, and kill only targets the immediate child despite group
creation. ProviderBridge pending response senders are retained until timeout;
there is no terminal subscription to fail concurrent or late RPCs immediately.
Host PluginsHost::shutdown says kill reaps although it does not prove reaping.

The current host Windows spawn_grouped helper is not a safe reusable answer:
`platform/windows.rs:307` explicitly spawns before assigning a job, permits a
post-spawn descendant escape window and silently falls back to direct-process
termination on job failure. The svc Unix helper discards kill errors; its
non-Unix helper spawns taskkill and waits without an absolute deadline. Neither
satisfies this issue's acceptance criteria.

## Proposed ownership and admission

1. Introduce a service-owned resident supervisor with tracked lifecycle tasks
   and exclusive child/pipe/tree custody. The host supplies its existing Tokio
   runtime handle; startup remains on the existing plugin setup background
   lane. Do not create an independent forever-live runtime or untracked thread
   per subscription. ResidentSession becomes an admission/control handle, not a
   shared Child or pipe mutex. A supervisor JoinSet/registry owns every task
   through successful reaping or an explicitly retained unresolved outcome.
2. Serialize each outgoing frame through a bounded writer adapter (existing
   MAX_LINE_BYTES = 1 MiB), so serde fails at the byte limit instead of first
   allocating an arbitrarily large string. Admit at most 32 ordered entries
   and 2 MiB queued bytes per session, plus one active <=1 MiB frame. QueueFull,
   TooLarge, Closed and WriteTimeout are typed transport failures. No implicit
   coalescing of requests, replies or callback notifications; overflow fails
   before admission. Partial writes keep one owned offset and absolute deadline
   (proposed 2 seconds) and are never retried from byte zero.
3. Cancellation/close/kill uses an independent control latch plus Notify, not
   the bounded data queue, so a stopped stdin reader cannot block it. The owner
   selects control, pipe readiness, child state and deadlines. Unix pipes must
   be nonblocking; Windows uses finite owned synchronous-I/O threads with repeated exact-thread cancellation (details below). No pipe read/write,
   child wait, thread join or potentially blocking mutex occurs on compositor
   callbacks. Every producer remains bounded and pulses the existing waker
   only when it publishes useful state.
4. ProviderBridge attaches to the session's terminal state atomically with
   pending insertion. Closing drains pending senders with a typed cause and
   prevents all later admission. A replacement owns a new session handle;
   stale responses cannot satisfy that handle's calls. This is session custody,
   not a claim to complete THE-165's wider event-generation contract.

## Lifecycle state machine

Running -> Closing(reason, absolute deadline) -> Settled { leader_reaped, pipes_settled, tree: Unproven }, or
Closing -> Held(error, retained custody) when actual owned resources remain unsettled.
Settled releases admission and permits replacement; an unproven tree guarantee
alone does not invent living descendants or block every ordinary restart.
Only the owner transitions state. Stdout EOF is protocol closure, not process
exit: close admission, fail pending RPCs, finish/cancel active output, request
termination and drain/close pipes within the same absolute deadline. Malformed
output, overlong lines, stdin failure, deadline expiry, explicit disable,
reload, shutdown and callback panic use the same terminal path. Deliver exactly
one terminal event after queued callbacks have ceased; callback panic is caught
at the owner boundary and cannot silently drop process custody. Worker launch
failure must happen before process creation or synchronously transfer custody
to an already-existing supervisor—never detach a fallback waiter.

A polite deactivate is admitted only if space/time remain; it cannot postpone
hard shutdown. kill returns control admission immediately, and a separate
completion receipt reports Reaped/Held. Host shutdown requests every session
first, then one off-loop coordinator awaits receipts against one absolute
application deadline (not N sequential per-plugin timeouts). Dropping a session
requests close and transfers observation to the supervisor registry, whose
handles remain tracked until application shutdown. An unresolved process is
reported as held, not 'shut down'. The application exit policy for held children
must be explicit; simply dropping the registry would violate retained custody.

## Platform seam and proof limits

A narrow svc process-lifecycle platform seam should replace copying the host's
best-effort wrappers. Its architecture ratchet exception must be scoped to the
new platform directory and justified in ARCHITECTURE.md; arbitrary cfg growth
in service logic remains prohibited. No substrate dependency moves into core.

Unix: create a fresh process group before exec, retain the unreaped leader until
all group signaling is complete, use non-reaping wait observation (waitid with
WNOWAIT where supported), inspect every syscall result and never signal a PID
once custody is lost. Reap only after termination admission and pipe cleanup.
Signal failure leaves a held owner; no PID-number delayed reaper fallback.
However a process group cannot contain descendants that call setsid/setpgid,
and reaping the leader does not prove all grandchildren were reaped. Full
escaped-tree assurance requires an enforceable containment primitive, e.g.
a Linux PID-namespace supervisor or suitably protected delegated cgroup, with
its own availability/failure policy. A best-effort /proc tree walk is not proof.
macOS cannot silently claim the Linux containment contract. This policy is a
remaining architectural choice; group ownership alone may ship a clearly scoped
liveness repair but cannot mark the complete THE-154 gate done.

Outstanding full-tree acceptance (not implemented by this scoped repair): create a kill-on-close non-breakaway Job Object and attach before the
child can run. This needs creation-time job-list attributes or a genuinely
suspended child with an owned primary-thread handle, followed by successful job
assignment and resume. Standard Command::spawn plus post-hoc assignment does
not qualify. Own/cancel every overlapped pipe operation and retain process/job
handles until completion or a held failure. Query job active-process accounting
and final process exit; a successful TerminateJobObject call alone is not a
reap certificate. No taskkill or silently disabled Windows support. The approved scoped repair retains direct-child Windows availability with tree=Unproven; it does not claim the full-tree criterion above. Existing host helper cannot provide these invariants.

## Scoped implementation and validation status

Implementation uses the host existing runtime and a registry capped at 32 sessions.
Writes have the approved frame/count/byte limits, terminal admission and pending
RPC closures share a race-safe latch, and provider replies route directly to the
originating session. A guard constructed before task submission transfers actual
objects to Held on cancellation or panic. Registry-owned join mutexes preserve
handles if shutdown waiters are cancelled or concurrent. A registration guard is
acquired before publishing a new entry. Final shutdown retries retained objects;
platform Process Drop issues only a nonblocking termination/cancellation request
through still-owned identity. Application exit cannot retain custody after death.

The Windows pipe implementation is deliberately not described as overlapped:
locally cached Tokio wraps standard Windows child pipes in blocking-pool I/O,
which is not an adequate cancellation proof. Owned dedicated workers remain
tracked until confirmed completion. Closing repeatedly requests cancellation
against their exact thread handles through one deadline, including the race where
ERROR_NOT_FOUND arrives before the worker starts its syscall. See Microsoft
[CancelSynchronousIo](https://learn.microsoft.com/en-us/windows/win32/api/ioapiset/nf-ioapiset-cancelsynchronousio)
and [cancellation races](https://learn.microsoft.com/en-us/windows/win32/fileio/canceling-pending-i-o-operations).
A deterministic pre-syscall barrier fixture is present but native Windows execution
is unavailable in this Linux environment and remains an outstanding gate.

The OpenSpec change `repair-resident-plugin-lifecycle` records the scoped repair
and separates full containment acceptance. Test execution evidence will be filled
in after the coordinated svc/host builds; source review is not a test pass.

## Full-issue verification still required

Pure/fake-I/O state tests: byte+count admission, FIFO partial writes, full-queue
control preemption, same absolute deadline, late RPC rejection, double shutdown,
EOF while alive, callback panic, injected spawn/write/wait/kill failures, and
simultaneous natural exit. These must exercise the same owner state machine.

Native Unix fixture children only: never-read stdin with >pipe-capacity output;
close stdout then sleep/ignore EOF; ordinary and escaping descendants retaining
both pipes; concurrent close/disable/reload/shutdown; natural-exit/kill races;
verify parent and complete contained tree identity disappear and ownership
never signals a recycled PID. Assert a compositor heartbeat advances while
all callbacks fail/admit within their bounded work budget. Use no live plugin
or live process signals. Native Windows equivalents must exercise job creation,
assignment/resume failure, non-reading child, retained descendant pipes,
cancellation, job completion accounting and handle cleanup. Cross-compilation
cannot substitute for these native outcomes.

Run focused resident-session/provider/host lifecycle tests and scoped `just
quick` after primary review. Native platform restrictions or missing containment
proof remain outstanding evidence. No queue-only fix, detached waiter, ignored
signal result or successful direct-child wait satisfies full acceptance.

## Exit-notice investigation during fixture validation

The first native suite exposed a missed Tokio SIGCHLD wake. Independent Python
and Rust diagnostics narrowed it to the sandbox denying UnixStream socket send
with EPERM: SIGCHLD delivery itself works, but Tokio's self-socket wake write is
rejected. This is an environment restriction, not evidence of an upstream Tokio
bug. The approved Linux repair now retains a pidfd in AsyncFd for exit readiness,
while still verifying a non-auto-reaping signal disposition and using WNOWAIT for
leader status/group identity. The exact-source pidfd harness observed exit in
about 60ms and passed identity-loss/final-drop fixtures 2/2 before the repeated svc
build. No periodic poll was added. Kernel ENOSYS/EINVAL explicitly falls back to
SIGCHLD; other pidfd/registration failures keep child custody for failed-start
cleanup. ESRCH prohibits later signals. Native non-Linux behavior is unverified.
See [pidfd_open contract](https://man7.org/linux/man-pages/man2/pidfd_open.2.html).

## Scoped evidence (2026-09-13)

The repeated native svc build completed without warnings. The complete plugin
subset passed **45/45** tests in 0.68s, including provider final-reply/late-close,
old-session reply isolation, byte/count/FIFO admission, non-reading stdin,
stdout-EOF-while-alive, callback panic, spawn failure/replacement, cancellation
before first owner poll, concurrent/cancelled shutdown, deterministic join-deadline
barriers, shutdown during spawn registration, consuming-wait identity loss and
final owned drop. Logs: `/tmp/thegn-maintenance-resident-second-build.log` and
`/tmp/thegn-maintenance-resident-final-tests.log`.

Independent exact-source platform harness passed **4/4** checks, including a
counting-waker fixture with zero exit-notice wakes during 60ms of a live child
waiting on stdin; releasing the fixture produced exit 7 while WNOWAIT still held
its identity. That counting-waker regression is now included in the repository
and will be part of final assembled validation. The initial three failing
fixtures and their actual fixes (queue preflight and pidfd readiness) are not
counted as passes. Windows cross type-check is running; native Windows execution
and escaped-tree containment remain outstanding.

## Final review revisions and remaining integration gate

The native plugin suite passed **46/46** after the lost-identity guards and idle
waker fixture (`/tmp/thegn-maintenance-resident-final-linux.log`). The complete
Windows svc library and test graph passed `cargo check --offline --locked -p
thegn-svc --target x86_64-pc-windows-gnu --tests` in 13.12s; the three existing
warnings are outside the lifecycle files. This compiles the Windows cancellation
fixture but does not execute it. The isolated Darwin exact-source platform and
embedded tests also cross type-checked; native macOS behavior is unverified.

Two final reviewed refinements follow that native checkpoint. BoundedFrame now
reserves exactly the missing newline byte before pushing it; an ordinary
provider.call-shaped JSON fixture previously grew from capacity 600108 to 1200216,
and now finishes at capacity 600109 within 1 MiB. The independent exact-source
serializer check passed. Final Unix ownership also rejects all numeric waits
following identity loss, and termination targets the still-owned leader after
original-group success or ESRCH without discovering another group. Both injected
group outcomes are tested using an owned shell builtin with no descendants;
lost/reaped identity reaches no later signal or wait syscall. The final actual-
source Unix platform harness passed **4/4**, including these guards, final drop
and zero idle wakeups. The integrating reviewer will run the final plugin suite
(expected 48 tests) with host integration and quick/clippy; that final graph is
not claimed by the earlier 46-test checkpoint.

## September 14: native fixture and CI follow-up

Primary approved a test-only repair of the native verification gap against main
`1ef8228f`. The Windows CI job selected service IPC tests and host platform tests,
but never the resident service lifecycle tests. The existing service fixtures use
`sh`, so adding their names to the Windows job would depend on a foreign shell.

The new `plugin::platform::native_tests` module reexecutes the Rust unit-test
binary with an exact ignored helper selector and a private child-only mode. An
explicit READY message proves the fixture reached its intended behavior before
admission/control assertions. The helper is not a regression pass and is not a
shipping executable. Every fixture has a 20-second process-local watchdog;
parent assertion unwinding requests cleanup through the retained supervisor.
There are no fixture grandchildren or broad process kills.

Eight native regressions cover a stopped stdin reader and calling-thread
progress, the write deadline, stdout EOF while the leader lives, native pipe and
protocol failures, callback panic, final reply and pending-RPC EOF, concurrent
shutdown/kill, and spawn failure followed by replacement. All settlement claims
remain `tree: Unproven`. The existing deterministic Windows cancellation test
is selected separately through the Windows tests module (currently one test);
cancellation between the worker
latch and ReadFile must be repeated until the actual thread finishes.

The opt-in Windows CI job runs both selections. Its opt-in policy, macOS full
build/test job, production platform code and plugin availability are unchanged.
No CI dispatch, push or native Windows/macOS execution is performed by this
change. Linux execution passed at the assembled follow-up checkpoint. The full
Windows service library/test graph, including the new fixture source, also passed
cross-compilation; the new Darwin fixture graph remains unverified.

| Gate                              | Evidence before this follow-up                                                           | Remaining evidence                                                                                     |
| --------------------------------- | ---------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------ |
| Native Linux lifecycle            | Eight new and 48 existing plugin regressions passed; two ratchets passed; helper ignored | Final follow-up integration/source checks                                                              |
| Windows library/tests compilation | Full service `x86_64-pc-windows-gnu --tests` crosscheck passed                           | New fixture source passed full Windows --tests crosscheck; native execution still required                                  |
| Native Windows lifecycle          | No recorded resident fixture execution                                                   | Run the explicit native CI selections on Windows, including CancelSynchronousIo race and failure paths |
| Darwin source compilation         | Isolated actual-source platform/tests crosscheck passed                                  | Added portable fixture source is not covered by that historical check                                  |
| Native macOS lifecycle            | No recorded resident fixture execution                                                   | Execute resident tests on macOS to exercise SIGCHLD/WNOWAIT and native pipes                           |
| Complete escaped-tree containment | No enforceable resident tree owner on any supported platform                             | Architectural containment prerequisite and native descendant/accounting evidence                       |

Complete-tree closure cannot be obtained by reusing the current host Windows
helper: it assigns the Job Object after spawn and permits a direct-child fallback.
Its existing tree-labelled tests observe the leader, not complete descendant
accounting. A pre-execution non-breakaway job owner and completion evidence are
still required. Linux process groups and pidfds do not contain setsid/setpgid
escape. The existing bwrap sandbox is a different launch path and permission
contract, not a drop-in resident owner; a PID-namespace or protected cgroup owner
requires a separate availability and escape policy. There is no existing macOS
complete-tree containment seam to extract. These are outstanding prerequisites,
not claims that an unproven tree is a known living descendant or a reason to
disable otherwise working plugins.

The native follow-up receipt is `/tmp/thegn-rolling-plugin-20260914.log` with
`/tmp/thegn-rolling-plugin-20260914-results.json`: 58 passed, one ignored (56 plugin
regressions and two ratchets). Independent reviewer verified that the eight new
tests passed and excluded the helper from the regression count. These are Linux
results, not execution evidence for Windows cancellation or macOS SIGCHLD.

The new Windows source gate passed `cargo check --offline --locked -p thegn-svc
--target x86_64-pc-windows-gnu --tests`, recorded in
`/tmp/thegn-rolling-windows-check-20260914.log`. Its three warnings are preexisting
and outside the new fixture source. This is typechecking, not native execution.
The older isolated Darwin harness covers only platform/unix.rs and does not cover
the new native_tests/session/provider fixture graph. A stubbed replacement would
not establish that graph's behavior, so Darwin source and native macOS execution
remain explicitly pending.
