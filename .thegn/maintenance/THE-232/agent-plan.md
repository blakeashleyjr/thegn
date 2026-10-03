# THE-232 plan

Scope: crates/thegn-host/src/agent_run.rs (unix runner), minimal pub(crate) visibility on
platform/sound_process.rs helpers (live_members, next_backoff) - sound path untouched.
Approach: reuse sound_process leader-pin (WNOWAIT) + group drain + killpg. Readers read to EOF
forever (retain only a bounded tail per stream, aggregate cap, truncation metadata), poll-based so a
stop flag ends them even if an escaped descendant holds the pipe. One end-to-end deadline
(timeout_secs, 0 => finite ceiling) covering exit wait, group drain, TERM->KILL, settle, reader join.
Typed AgentRunOutcome (Exited/TimedOut/Cancelled/DescendantsRemain/Reap/Spawn); run() keeps bool.
Unreaped leader on settle failure is handed to a reaper thread (no zombie).
Tests: >1MiB stdout/stderr/both, early exit with bg descendant holding pipes, TERM-resistant
descendant, timeout 0 ceiling clamp, cancel during run, no leftover group.
Out of scope: Windows job teardown (stub stays), wiring a global shutdown flag, caller changes (callers already treat the bool as advisory).

Review follow-ups (batch 10):

- After the leader exits the rest of its group gets QUIESCE_AFTER_LEADER = 15 s (capped by the overall deadline), then TERM->KILL; the leader stays unreaped (WNOWAIT) until the group is gone. Non-Linux `live_members` falls back to `!group.is_empty()`, so on darwin the run always waits the quiesce window and then kills - documented, accepted.
- `Stop::Supervise` (waitid failed): the group is NOT signalled (pgid no longer pinned); the outcome is reported as Reap(e).
- `timeout_secs = 0` is a 6-hour ceiling; values over 30 days clamp.
- Cancellation is NOT wired to thegn shutdown (run() passes a static NEVER flag). Follow-up.
