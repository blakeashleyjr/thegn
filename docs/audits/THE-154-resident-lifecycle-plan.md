# THE-154: resident plugin lifecycle investigation and proposed repair

Status: primary plan review requested; no implementation approved or claimed.
This is an existing lifecycle defect, not a plugin feature expansion.

## Current reachable defects

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
   be nonblocking; Windows uses cancellable overlapped I/O. No pipe read/write,
   child wait, thread join or potentially blocking mutex occurs on compositor
   callbacks. Every producer remains bounded and pulses the existing waker
   only when it publishes useful state.
4. ProviderBridge attaches to the session's terminal state atomically with
   pending insertion. Closing drains pending senders with a typed cause and
   prevents all later admission. A replacement owns a new session handle;
   stale responses cannot satisfy that handle's calls. This is session custody,
   not a claim to complete THE-165's wider event-generation contract.

## Lifecycle state machine

Running -> Closing(reason, absolute deadline) -> Reaped(result), or
Closing -> Held(error, retained custody) when the OS cannot prove termination.
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

Windows: create a kill-on-close non-breakaway Job Object and attach before the
child can run. This needs creation-time job-list attributes or a genuinely
suspended child with an owned primary-thread handle, followed by successful job
assignment and resume. Standard Command::spawn plus post-hoc assignment does
not qualify. Own/cancel every overlapped pipe operation and retain process/job
handles until completion or a held failure. Query job active-process accounting
and final process exit; a successful TerminateJobObject call alone is not a
reap certificate. No taskkill, direct-child fallback or silent unsupported-
Windows regression. Existing host helper cannot provide these invariants.

## Verification required before greenlight for landing

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
