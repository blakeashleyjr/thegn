# THE-219 neighboring retry audit

Source audit at drain candidate `cce73f40fa2660dd95e20d683fe49939b5e1d604`.
This supplements the earlier Part A source review rather than repeating it.
Only driver-local progress and its direct CLI/UI dispatch are assessed; no
provider, CLI, Git workload or Cargo execution was performed for this audit.

## Every branch-local edge

`merge_driver::drive_queue_with` has one selected-item `for` (line228), one
branch-local `loop` (263), and exactly one production `continue` (430).
`folding` is published before the inner loop, once per selected item.

| Path                                                    | Next edge                     | Progress or termination reason                                                                                                                                                                     |
| ------------------------------------------------------- | ----------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Attempt returns Err                                     | break                         | One needs_human outcome; no same-item retry.                                                                                                                                                       |
| Landed / UpToDate / Ready                               | break                         | Settled outcome; any lifecycle work occurs once on this path.                                                                                                                                      |
| Unreachable / GateError                                 | break                         | One deferred/gate_error result; no agent admission and no immediate retry.                                                                                                                         |
| Conflict / GateFailed, InfraHold                        | break                         | One agent_blocked transition and one deferred result; attempt count unchanged; outer loop reaches the next selected item.                                                                          |
| Conflict / GateFailed, Run or RunDegraded               | continue after runner returns | Admission is followed by a strict local attempt increment, agent_running transition and one synchronous runner call. RunDegraded's warning alone does not retry: it shares the same budgeted path. |
| Conflict / GateFailed with no agent or exhausted budget | break                         | Terminal deferred/gate_failed/needs_human result; no retry.                                                                                                                                        |

The sole immediate retry is guarded by `use_agent && agent_runs <
agent_max_attempts` (374). `agent_runs += 1` (407) therefore cannot overflow its
u32 type, even at a u32::MAX ceiling, and strictly decreases the remaining
budget. `use_agent` was computed from a present immutable resolved command, so
the conditional template lookup before the runner is not a path that repeatedly
skips both action and budget. The callback's advisory false result, missing
prompt, spawn failure or a source-unchanged agent does **not** refund attempts.
The next fold is the verdict; it need not prove the agent changed source.

Given returning attempt/admission/runner calls, a selected item performs at most
`1 + max(0, ceiling - initial_attempts)` folds and at most that remaining budget
of runner calls. The bound is mathematical (not a new unchecked u32 addition).
Best-effort DB writes can fail, but the current invocation's local counter still
advances. This does not prove durable retry accounting across failed DB writes
or competing drains, and no such guarantee is added here.

## Direct dispatch and independent retry

- `agent_run::agent_floor_gate` (72–105) returns exactly one Run, RunDegraded or
  InfraHold from one current resolution. It has no admission retry loop.
- CLI `cmd/merge.rs:570–634` captures current non-ready/non-landed rows once and
  calls the driver once. Its target-stamp `for` advances through that fixed
  vector. A later plain drain re-enumerates agent_blocked along with other
  non-settled statuses and preserves the stored attempts. `merge retry`
  (303–315) deliberately resets/requeues and returns; it does not run a drain.
- UI `handlers/merge_queue.rs:111–175` captures rows and invokes the same driver
  once in its worker. `arm_fold`/`dispatch_drain` (187/254) gate an explicit new
  dispatch on the in-flight flag. Step handling (327 onward) updates projection;
  Done clears the flag without scheduling another drain. Its `try_recv` loop
  consumes one message each iteration, not another branch attempt.

This is not a global concurrency, cancellation, process-tree or wall-clock
completion proof. `agent_run::run` retains blocking child/pipe joins and a
separate watchdog; zero timeout disables that watchdog. Those existing
THE-232/THE-211/related runtime obligations are not repaired or waived by this
finite branch retry audit. A callback that never returns can still block one
attempt. No new production change is proposed from this audit.

## Evidence and remaining gates

The actual Part A native retry log records8/8 status tests passing
(nextest816c3109-e2ee-4d3f-87dd-1b14cce6a09b):
`/tmp/thegn-the219-part-a-native-retry-20260914.log`. Its four new tests cover
both hold causes, same-DB floor recovery, no-op agent budget exhaustion and
GateError continuation. EVERY-status-UPDATE and independent budget triggers
include identical repeated status writes. The original compile failure and
narrow i64 SQL fixture correction remain recorded separately.

The exact InfraHold break→continue counterfactual at c3e4cd55 failed both
conflict/red-gate selectors on the second held fold, as intended (0.436s,
30s outer watchdog), per `/tmp/thegn-THE219-counterfactual-native-receipt-20260914.json`
and its retained raw log. No infinite actual-CLI counterfactual was executed.

Part B actual private CLI conflict and red-gate tests passed in the combined
focused and full native runs. Both complete original phase/SQL/cleanup receipts
are retained in the evidence directory; each records private-root removal.
Part B proves branch repair then independent plain-drain re-enumeration; Part A
separately proves recovered floor admission. Current strict Clippy and metadata validation passed; reviewed implementation landed on local main at
`c0d3d860a22db0a7fddafb3d338bf40b25248ade`. Neither
this audit nor these fixtures claim THE-608's broader diagnostic acceptance,
live user queue behavior or successful real sandbox establishment.

## Segmented native evidence and remaining gates

[The evidence manifest](maintenance-04-2026-09-15/manifest.json) preserves original receipts, logs and independent reviews byte-for-byte. Combined `62335026` focused acceptance passed 68/68. Its full configured native run completed 8377 tests: 8376 passed, one host-key-literal ratchet failed, and 26 configured tests were skipped. Test-only `a4468a8e` replaces the precedence fixture's HostKeyAlias argument with distinct ConnectTimeout values; its fresh six-test gate passes all five precedence tests and the formerly failing ratchet. These are overlapping, segmented gates, not a single clean full-a446 run.

Source ratchets, 159 strict OpenSpec items, treefmt and non-Rust lint passed. Final metadata ratchets/OpenSpec checks and strict offline workspace/all-target Clippy also exited zero; Clippy took 13m58s, with inherited dependency warnings retained in the raw log. [The final gate receipt](maintenance-04-2026-09-15/thegn-maintenance04-final-gates-receipt-20260915.json.raw) records exact commands, source and log hashes. Reviewed implementation landed on local main at `c0d3d860a22db0a7fddafb3d338bf40b25248ade`. [The landing readback](maintenance-04-2026-09-15/thegn-three-fix-local-main-readback-20260915.json.raw) verifies all 16 reviewed Rust hashes, the 40 pre-landing evidence payloads and preserved user files. Tracker closure/readback remains pending; no new performance acceptance is claimed.
