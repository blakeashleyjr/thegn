# THE-232 — primary instructions: ADVERSARIAL REVIEW (batch 10)

Review `git diff main...HEAD` (fix commit f3c719cfe; plan .thegn/maintenance/THE-232/agent-plan.md).
NOT compiled yet — flag likely compile errors. Do not change production code; write findings only.

Change: crates/thegn-host/src/agent_run.rs new `bounded` module — readers drain to EOF keeping a
32 KiB/stream, 48 KiB total tail with truncation metadata; 100 ms-wake readers with a stop flag + 500 ms
grace for escaped pipe holders; the leader is observed with WNOWAIT and kept unreaped until the group
quiesces (live_members with backoff), then TERM, 2 s, KILL; deadline = timeout + ~3.5 s cleanup;
timeout 0 -> 6 h ceiling, >30 days clamped; typed AgentRunOutcome; a reaper thread takes the wait if the
leader is not reapable after the KILL settle; run() still returns bool. platform/sound_process.rs: only
visibility changes (POLL_INTERVAL, DRAIN_BACKOFF_CAP, live_members, next_backoff -> pub(crate)).

KEY QUESTION: the queues' agent handoff is wrapped by sandbox_cpucap::wrap_background_argv (a
systemd-run transient scope). When wrapped, is the process-group leader systemd-run while the agent runs
in a transient scope NOT in our pgid? Does killpg reach the agent at all, does live_members see it, and
would a timeout leave the agent running in its scope? Trace the actual argv and spawn path.
Also check: WNOWAIT ordering (never killpg after reaping); the reaper-thread fallback is bounded; the
reader poll implementation (no busy loop; partial UTF-8 in tails); timeout 0 previously meant "never" —
is the 6 h ceiling a behaviour change merge/pr-queue agent hooks rely on; typed outcome discarded at the
bool boundary; shared-constant visibility changes don't alter sound behaviour; tests realistic and not
flaky under load, /proc use guarded to Linux; ignored-result and platform-cfg ratchets. Every spawned
child must be reaped (CLAUDE.md). Verdict source-review-clear or revisions-needed with file:line findings.
