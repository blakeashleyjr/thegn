# THE-311 plan

Scope: bridge server proc.spawn / proc.kill / connection teardown (crates/thegn-svc/src/bridge/mod.rs), helpers in plugin/proc.rs.

- Spawn each child as a process-group leader (set_process_group).
- Per-channel ProcCtl (pid, phase, condvar). Waiter does waitid(WNOWAIT), killpg SIGKILL, then reap under the phase lock; signals are only sent while phase is Running, so never killpg after reap.
- proc.kill: runs on a worker thread; SIGTERM, bounded wait, SIGKILL, bounded wait; typed ProcKillOutcome (terminated/killed/gone/timeout). proc.exit is written before the response, so the client keeps its subscription until the terminal event.
- Connection close: SIGKILL all groups, bounded wait for reap + exit announce.
- Channel-id reuse: reject live duplicate; waiter deregisters only its own generation (Arc::ptr_eq).
- Out of scope: Windows job objects (blocked on THE-274), exec/exec.batch bounded children, BridgeClient construction (THE-313).
- Tests: EOF-ignoring child, TERM-ignoring tree, pipe-holding grandchild, unknown channel, channel reuse, transport loss.
