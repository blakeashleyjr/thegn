**Independent session, sandbox and terminal review — September 13, 2026**

This review independently checks findings 1, 5 and 10 in [the original audit](live-build-2026-09-13.md), traces the affected launch/reconnect/teardown paths, and compares source against `9a601c53..f4c1355b`. Session restoration is a confirmed regression. Host fallback requires more careful attribution. The historical terminal panic is reproducible in an isolated dependency fixture, with a correction to the original audit's exact failing statement. An additional source defect can bypass an explicitly configured isolation floor.

| Original finding                                           | Review disposition                                                                                                                                 | Priority                                                |
| ---------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------- |
| 1 — missing daemon session restoration                     | Confirmed; the same missing absence capability also affects provider recovery                                                                      | High                                                    |
| 5 — host fallback                                          | Confirmed backend-resolution warnings; revise the implication that seven warnings mean seven successfully launched, unexpectedly uncontained panes | Medium operational observation                          |
| 10 — terminal teardown panic                               | Confirmed historical crash and reproduced dependency behavior; failing line is `flush().unwrap()`, not `exit_alternate_screen().unwrap()`          | Medium; fix before treating hangup handling as reliable |
| Additional — isolation floor bypass                        | Confirmed source path; pre-existing; not demonstrated in the user's current panes                                                                  | High when a fail-closed floor is configured             |
| Additional — reconnect errors exhaust recovery immediately | Confirmed source path; no captured live reproduction                                                                                               | Medium                                                  |

Only the review artifact and isolated `/tmp` fixture files were written. No live configuration, sessions, database records, processes, tickets, or production code were changed. The user's modified `justfile` was preserved. No application build or full test suite was run.

**1. Missing-session restoration: confirmed, with broader affected scope**

The four original warnings remain independently readable in retained host log rotation `thegn.log.1` at lines 54210, 98870, 98872 and 98885. They name panes 13, 40, 41 and 42 and report `attach refused: not found: session …`, followed by refusal to open a duplicate shell. They occurred at 14:58:15 and 16:35:11 in run `tlbp111dis`. Rotation has changed the filename since the original capture; the line positions currently match its old `thegn.log` references.

The complete relevant path is:

1. `crates/thegn-host/src/panes.rs:543` constructs `LazyDaemonSource` for every daemon-backed stream pane.
2. `daemon/client.rs:417` implements its attach through lazy daemon discovery and `attach_session`.
3. After an attach error, `pane.rs:1057` calls the trait's `session_absent` method. Errors are conservatively mapped to false.
4. `pane_source.rs:37` defaults that method to `Ok(false)`. `LazyDaemonSource` never overrides it (`daemon/client.rs:412`).
5. The actual daemon implementation at `daemon/client.rs:294` performs a successful session-roster request and checks the identifier, but production's lazy wrapper does not reach it.
6. The relay emits a failed-output message and `PaneEvent::Exit(id, Some(1))` at `pane.rs:1103`, rather than opening the fresh fallback spec.

Commit `d934df80` introduced the trait method and both absence gates, and implemented the roster query only on `DaemonSource`. The lazy wrapper already existed. This establishes a regression within the audited build range, rather than merely inferring one from log wording. Existing relay tests use a fake source that explicitly returns true (`pane.rs:1812`), so they do not exercise production delegation.

The defect affects initial restoration and reconnect-after-stream-loss: the second absence gate is `pane.rs:1201`. It does **not** mean every restored pane fails; a session that is still present and attaches successfully bypasses the failing fallback branch. A fresh session also cannot recover the previous shell's in-memory state; it only restores a usable pane.

Broader source finding: `ProviderSource` at `pane_source.rs:84` also inherits the false default. Consequently a genuinely expired native provider session cannot use either fresh-session recovery path. Native attach is implemented for Sprites in `crates/thegn-svc/src/provider.rs:1974`; the source wrapper can additionally select its iroh transport. There is no captured provider incident here, and no basis for calling a provider/network failure observed in this run. This is a source-level expansion of the same regression, not another counted operational failure.

Minimal remediation: forward the authoritative daemon query through the lazy source; retain fail-closed handling of roster/transport errors. For providers, add a typed, authoritative missing-session result or supported roster query before enabling fresh opens. Do not equate arbitrary attach errors with absence. Cover the **actual lazy wrapper** in tests: reachable daemon with missing session opens once and emits `SessionFallback`; existing session does not open another shell; unreachable/failed roster does not open; reconnect-after-loss uses the same policy. Add corresponding provider cases when the protocol can prove absence.

**5. Host fallback: confirmed resolution, narrower incident claim**

All seven original warnings remain in `thegn.log.1`: 54199, 54220, 98860, 98862, 98863, 98902 and 99486. They occurred from 14:58:14 through 16:35:41. The warnings establish backend selection falling through to host. They do not independently establish seven completed process launches: several occur immediately before the failed reattachments described above.

The production preparation loop emits one notice at its final host outcome (`crates/thegn-host/src/agent.rs:836`), after attempting candidates. `SandboxOutcome { spec: None, backend_label: "host", … }` is returned at line 842. Launch composition treats the absence of a sandbox spec as the host path, and persists observed backend metadata at line 3634. The fact that a pane uses the daemon is **not itself** evidence of host execution: daemon panes can carry already-composed sandbox argv (`panes.rs:504`).

Global `backend = "bwrap"` is not enough to diagnose these particular opens. `agent.rs:323` permits deliberate per-terminal/backend choices to override configuration, while ordinary saved values only override auto. An explicit usable bwrap selection is attempted alone; failures at resolution or ensure return an error (`agent.rs:713`, `722`), rather than silently entering the auto chain. Environment resolution also supplies per-environment sandbox configuration. The global bwrap setting therefore does not prove that bwrap was requested or failed for all seven warnings.

The warning's “installed but not running” phrase is stronger than its evidence. `crates/thegn-core/src/sandbox_backend.rs:290` combines local executable presence with an availability result other than `Present`. Failed access, runtime errors or restricted probing can produce this result; it does not prove the system service was stopped. The correct incident claim is “installed backend reported unavailable to the probe.”

The original captured doctor configuration reports `isolation_floor = "off"` and `on_floor_miss = "degrade"`. That copied-state output does not resolve every pane's effective settings or prove live runtime health, but it also supplies no evidence of a configured fail-closed promise being violated in this run.

Minimal remediation: correlate each launch with effective backend choice, environment, policy and observed execution result; distinguish expected explicit-host/auto outcomes from failures of a requested boundary. Change availability diagnostics to report the actual probe failure. Verify bwrap, auto, saved explicit host, and unavailable-runtime cases using controlled launch fixtures before changing the user's runtime or backend selection.

**Additional high-priority source defect: host fallback can bypass the isolation floor**

`prepare_sandbox_env` checks the isolation floor only after it has a non-host sandbox spec (`agent.rs:645`). Its final fail-closed guard at line 740 tests whether a previous candidate populated `floor_miss`; it does not evaluate the actual final host result.

A concrete reachable configuration is local execution, sandbox enabled, `backend = "auto"`, a chain containing unavailable runtimes followed by `none`, `isolation_floor = "shared-kernel"`, `on_floor_miss = "fail"`, and the ordinary warning policy for missing backends. No explicit-host override is necessary:

1. Unavailable candidates produce no spec, so no floor comparison occurs.
2. Local `Backend::None` also produces no spec (`crates/thegn-core/src/sandbox.rs:726`).
3. The caller recognizes the `none` candidate and breaks (`agent.rs:720`).
4. `floor_miss` is still `None`; `explicit_choice` is false; the remote halt branch does not apply to a local placement.
5. Auto is not treated as an explicit containment demand by the dormant-runtime prompt (`agent.rs:782`). The caller returns the host outcome at line 842.

A candidate that initially meets the floor but fails during `ensure`/preflight also leaves `floor_miss` empty (`agent.rs:692`), so it can reach the same final host outcome. The `degrade` floor policy similarly misses its floor-specific warning on these paths. A non-local `Backend::None` result returns even earlier, before the floor comparison (`agent.rs:578`); that path needs its own comparison against the actual placement's isolation class.

This defect is already present in `9a601c53` and remains in `f4c1355b`. It is a conditional security-policy failure, not proof of exploitation or a measured isolation breach in the current run. Source search found no downstream pane-launch floor check that repairs the omitted decision. The separate queue-task gate in `agent_run.rs` is a different path and should not be inferred to share this defect.

Minimal remediation: evaluate the final execution class before **every successful preparation return**, including local host fallback and non-local bare execution. Preserve provider-managed bypass semantics. A failed stronger runtime must be compared against the backend actually chosen afterward. Required meaningful launch tests: all candidates absent; a floor-satisfying candidate fails preflight; explicit host under a demanded floor; non-local bare execution; `fail` refuses before child spawn; `degrade` emits a floor miss; floor-off preserves existing behavior. This review establishes the control-flow defect by source tracing; it did not execute these full launch cases.

**Additional medium-priority source defect: reconnect retry budget does not cover failed attaches**

After an established stream drops, `pane.rs:1178` computes a bounded backoff and attempts one reattach at line 1191. If attach fails and absence is false or unknown, control reaches the exit at line 1238 immediately. The outer loop continues only when an attach or fresh open succeeds. `MAX_DEAD_RECONNECTS` therefore bounds streams that repeatedly attach successfully and then drop; it does not retry an unavailable transport several times.

This behavior predates the audited range, although the new absence requirement makes an unknown/missing session take the exit path instead of the old unsafe fresh open. The source can emit “gone after reconnect attempts” following only one failed reattach, while the server-side shell may still be alive. This is distinct from the lazy-wrapper bug and remains after delegation is repaired.

Minimal remediation: retry transient attach/roster failures within an explicit time/attempt budget while continuing to prohibit unproven fresh opens. Keep close/detach cancellation responsive. A meaningful fixture should make attach fail twice and then succeed, assert reuse of the same session and zero fresh opens, then test budget exhaustion and pane closure during backoff. No such transient failure was established in the captured live records.

**10. Terminal teardown: confirmed and independently reproduced**

All ten retained crash report text files have the same EIO signature at `termwiz-0.23.3/src/terminal/unix.rs:539:28`. Nine precede the previous build; the September 13 14:20:36 report identifies `9a601c53`. No new crash report was present in the retained crash directory during this review.

Correction: line **539 is `self.write.flush().unwrap()`** in the installed dependency source. `exit_alternate_screen().unwrap()` is line 538. Other panic-capable destructor operations include DEC reset writes (523), modify-other-keys (537), and final termios restoration (543). Fixing only one call does not make the destructor safe.

Current `Cargo.lock:8229` still selects termwiz 0.23.3. `run.rs:1209` executes the non-panicking restoration callback and clears it, then calls termwiz's best-effort alternate-screen and cooked-mode operations (1211–1212). The terminal owned by the buffer still drops afterward. Frame-write retries in `frame_write.rs:20` mitigate transient output errors; permanent output failure still reaches teardown. The standalone attach input owner also drops a terminal after best-effort cleanup (`cmd/attach.rs:197`), so remediation should cover both owners.

An isolated experiment linked a tiny Rust executable directly to an existing cached termwiz 0.23.3 rlib. It opened a fresh PTY, entered raw/alternate mode, and used a pipe barrier to let its Python parent close only that fixture's PTY master before cleanup. It then called `exit_alternate_screen`, `set_cooked_mode`, and dropped the terminal inside `catch_unwind` so the experiment could record the result:

| Fixture condition | Explicit cleanup                                                    | Destructor result                                                |
| ----------------- | ------------------------------------------------------------------- | ---------------------------------------------------------------- |
| PTY intact        | Both calls succeed                                                  | No panic                                                         |
| PTY master closed | Alternate-screen call succeeds; cooked-mode restoration returns EIO | Panic at `unix.rs:539:28`, EIO — matching retained crash reports |

The alternate-screen success is consistent with termwiz buffering writes before the failing flush. This is direct evidence that explicit best-effort cleanup does not prevent the dependency panic. Catching this fixture panic is an observation technique, **not** the proposed production fix: a panic during an existing unwind can abort, and skipped destructor cleanup can leave signal registrations or terminal state unresolved.

Fixture source: `/tmp/thegn-termwiz-drop-review.rs`; runner: `/tmp/thegn-termwiz-drop-review.py`; results: `/tmp/thegn-termwiz-drop-review-results.json`. Compilation used `rustc --edition=2024`, `--extern termwiz=target/debug/deps/libtermwiz-23e19615750eb531.rlib`, and `-L dependency=target/debug/deps`. No Cargo invocation, application build, live terminal, application signal, or live process was involved.

Minimal remediation: use a controlled dependency patch or terminal owner that makes **all** destructor I/O best-effort while still unregistering signals and attempting termios restoration. Retain the existing independent panic-restoration guard. Verify healthy exit, permanent PTY hangup, transient EIO, early initialization failure, and destruction while unwinding in an isolated subprocess; the acceptance criterion is no secondary destructor panic or abort, with normal restoration preserved. This fixture verifies the selected dependency behavior, not a new crash in the running `f4c1355b` application.
