# Ownership and limits

One host-owned ResidentSupervisor uses the existing Tokio runtime and survives
plugin-host reloads. It reserves at most 32 entries before creating processes.
Each SessionWriter admits at most 32 FIFO frames and 2 MiB queued bytes, plus one
active frame of at most 1 MiB inclusive of newline. Serialization stops before
exceeding that allocation cap. Producers never own a pipe or wait for a child.
Close is a separate latch and Notify; it cannot be blocked by a full data queue.

Each owner handles nonblocking Unix I/O or finite, owned Windows pipe threads.
Windows synchronous I/O cannot safely be delegated to an untracked shared blocking
pool: cancellation can arrive before the syscall. Closing repeatedly calls
CancelSynchronousIo against unfinished, still-owned dedicated thread handles,
under one deadline, and joins only confirmed-finished threads. ERROR_NOT_FOUND is
not a completion receipt. Pre-spawn idle workers disconnect and join off the UI
lane if thread admission or process creation fails.

Unix creates a process group before exec and verifies SIGCHLD is not configured
to discard child identity. Linux >=5.3 retains a CLOEXEC pidfd registered with
AsyncFd for exit readiness; it never reads the pidfd or treats it as a tree
certificate. ENOSYS/EINVAL retains the explicit SIGCHLD fallback for older kernels;
other setup errors retain the spawned child for failed-start cleanup, and ESRCH
forbids later identity-based signaling. Non-Linux uses SIGCHLD readiness with
interest registered before status observation. The local sandbox rejects the
Unix-socket send used by Tokio's SIGCHLD self-wakeup (EPERM), while actual signal
delivery and pidfd readiness work; the pidfd path avoids that environmental
restriction without an idle timer. Non-Linux native behavior remains unverified.

Unix creates a process group before exec. waitid(WNOWAIT) observes exit without
releasing leader identity; group signaling precedes the consuming wait. An
ECHILD/consuming-wait identity loss forbids all later signals and numeric waits.
Original-group success or ESRCH is followed by a kill through the still-owned
unreaped Child, because the leader may have moved out of that group. Other group
errors remain reported; no newly discovered group is signaled. Neither process
groups nor direct-child Windows termination prove tree containment.

An owner guard is created before the future is submitted. Cancellation, including
before first poll, transfers still-owned Process objects to Held registry custody.
Join handles remain in registry-owned asynchronous mutexes while shutdown waits;
cancelling a waiter cannot detach them. Registration owns that mutex before the
entry is published. All sessions receive shutdown before concurrent waits use one
absolute application deadline. Final retry uses only retained objects, and final
platform Drop makes a nonblocking last termination attempt through retained
identity; it does not claim to reap an unconfirmed process or settle Windows I/O.

Leader reaping, pipe settlement and tree guarantee are separate receipt fields.
Settled releases actually settled ownership/admission and permits restart even
though tree=Unproven. Held retains actual unsettled resources; task join failures
also remain reportable. The host awaits cleanup outside the event loop on all
return paths and returns an error for unresolved owned resources. Custody cannot
survive application process exit; no report claims otherwise.

Provider correlation belongs to the originating session. Admission closure and
pending insertion synchronize their latches; explicit cancellation immediately
fails pending requests. Natural exit closes new admission, drains already-written
stdout for at most 100ms, routes responses directly to that session's bridge, then
fails unanswered calls. Host events never route a stale response into the current
bridge by plugin name. Wider stale plugin-event generations remain THE-165.
