**Live-build audit — September 13, 2026**

**Review follow-up:** three subagents independently reviewed these findings, with a separate build/queue review. See the [review synthesis](live-build-2026-09-13-review.md) for updated priorities, reproductions and corrections. The sccache override is present in the working tree at follow-up; the termwiz panic is at its final `flush().unwrap()`; performance timestamps generally end at writer submission rather than physical flush.

The new build is running without a recorded host/daemon ERROR or new crash in the captured window. Database checks pass. It still has a confirmed session-restoration regression, unsafe input logging under `just live`'s default debug filter, failing GitHub PR refreshes, and a failing delivery gate. A successful launch does not close those issues.

This is an operational audit plus targeted source and issue reconciliation. It is not a line-by-line security review of all changed files or a claim that every open tracker issue was reproduced.

**Scope and evidence**

All displayed times are America/Los_Angeles unless stated otherwise. Counts use a fixed log capture ending at **16:37:06**, with a supplemental live-log check through **16:47:50**. The supplemental window also contains zero host/daemon ERROR records, but adds the performance and thermal warnings described below.

| Item                           | Verified value                                                                                    |
| ------------------------------ | ------------------------------------------------------------------------------------------------- |
| Previous running build         | `9a601c53`, embedded build time September 2, 12:24:16; identified by its last crash report        |
| Current release binary         | `f4c1355b`, embedded build time September 13, 14:40:26; executable completed at 14:58             |
| Current host run               | `tlbp111dis`, started September 13, **14:58:13**                                                  |
| Current daemon run             | `tlbp121rbc`, started **14:58:14**                                                                |
| Source change range            | `9a601c53..f4c1355b`: 33 commits, 777 files, 44,079 insertions and 12,584 deletions               |
| Current-run host log records   | 29,996 DEBUG; 618 INFO; **3,587 WARN; zero ERROR**                                                |
| Current-run daemon log records | 16 DEBUG; 11 INFO; **zero WARN/ERROR**                                                            |
| Retained log scan              | 12 host/daemon log files, approximately **221 MiB**, plus current stderr and 10 crash reports     |
| Database                       | Schema **68**, matching source; SQLite online backup passed `quick_check` and `foreign_key_check` |
| Issue inventory                | All **612** nonarchived THE issues paginated: 107 Done, 81 In Progress, 117 Todo, 307 Backlog     |

Host log retention begins September 12 at 04:57:26. It does **not** cover the entire September 2–13 interval. The old daemon's retained rotations contain a very large September 7 burst, not continuous history back to September 2. The previous build's startup is no longer retained. Before/after performance samples have different workloads and durations.

Evidence files are under `/tmp/thegn-live-audit-20260913/` (private directory), including `audit-evidence.json`, `summary.json`, `performance.json`, `old-performance.json`, `doctor.json`, `config-validate.log`, `ratchets.log`, and `independent-ratchets.json`. Log line references below identify the capture; live rotation can later change those files. Raw typed characters are deliberately excluded from this report.

**1. High — restored panes cannot recover a missing daemon session. Confirmed regression.**

Four current-run reattachments failed: pane 13 at 14:58:15, then panes 40, 41 and 42 at 16:35:11. Each error says the daemon session was not found, followed by “refusing to open a duplicate shell.” All four produce a `process_failed` notification with exit code 1. Affected workspaces include the Sage project, thegn, and holdem-rs.

The root cause is visible in source. [Panes::spawn_daemon_backed](../../crates/thegn-host/src/panes.rs) constructs `LazyDaemonSource`. [ExecSource::session_absent](../../crates/thegn-host/src/pane_source.rs) defaults to `Ok(false)`. [DaemonSource](../../crates/thegn-host/src/daemon/client.rs) implements the authoritative roster check at line 294, but **LazyDaemonSource's implementation at line 412 does not forward it**. Consequently [relay_exec](../../crates/thegn-host/src/pane.rs), line 1056, always takes the refusal path for this source after a failed attach. The newly added absence requirement prevents the old fresh-session fallback from running.

Evidence: `thegn.log:54210`, `98870`, `98872`, `98885`; notification records at `54214`, `98879`, `98880`, `98892`. Follow-up should forward the authoritative check through the lazy source and test both a genuinely missing session and an unreachable daemon. Preserve the prohibition on duplicating a still-live shell. Related completed issues [THE-84](https://linear.app/blakeashley/issue/THE-84) and [THE-85](https://linear.app/blakeashley/issue/THE-85) do not establish that this new path works.

**2. High — default live diagnostics record ordinary typed characters. Confirmed, pre-existing behavior.**

The current capture contains **139 `thegn::input` key-dispatch records**, including ordinary character keys with no modifiers and no matched application action. [run.rs:19917](../../crates/thegn-host/src/run.rs) records both `raw_key` and `norm_key` whenever DEBUG is enabled. `just live` supplies a broad debug filter, so this does not require an explicit input-debug opt-in.

These records can preserve typed commands or credentials even when a pane application suppresses terminal echo. This audit did not reconstruct input or establish that a credential was typed. The files are mode 0644, but their enclosing state directory is mode 0700; there is no evidence here of access by another user or external disclosure.

Follow-up: suppress printable input values by default, require a separate explicit diagnostic opt-in if needed, and retain modifier/action metadata without the characters. The default live filter can also exclude `thegn::input=debug`. Do not share raw live logs as a routine support artifact without reviewing this field.

**3. High — GitHub PR refresh fails repeatedly for the active Sage repository. Confirmed, also present before rebuilding.**

There are **554 failures**, split evenly between `pr_list` and `pr_status`. Every captured continuation identifies the same error: GitHub cannot resolve repository `SageHealthyRCM/mysage2`. The local origin still names that repository. The database's cached PR-list timestamp for this project is **August 31, 07:54:19**.

Evidence: `thegn.log:54477–54479` and repeated matching request failures. The old retained run also contains 4,904 native-forge request warnings; this is not first observed in the new build.

The remote may be inaccessible to the selected token, renamed, or removed; this sandbox cannot independently verify GitHub access. Do not infer a deleted repository from GitHub's access-obscuring response alone. [native.rs:327–347](../../crates/thegn-svc/src/forge/native.rs) additionally classifies the observed octocrab error as `ForgeError::Other`, whereas its separate successful-response-with-errors branch promises CLI fallback. That makes the documented fallback unreliable for this observed error shape.

Follow-up: verify the selected account/token against this exact repository, show a clear repo-specific refresh error/freshness indicator, classify typed GraphQL failures consistently, and back off repeated permanent failures. Do not globally label this as an internet outage.

**4. High — the source gate remains red, and a delivery change is held in the queue. Confirmed known issue.**

`JUST_TEMPDIR=/tmp just ratchets` exits **1** at `delivery-check`:

```text
active OpenSpec changes missing from delivery index: preserve-config-validation-severity
```

The remaining **15** noncompiling commands from the expanded recipe, including delivery fixtures, source ratchets and idle-poll checks, pass when executed independently. This does not make the overall gate green.

The snapshot contains 34 merge-queue rows: 33 `landed` and one `gate_error`, for `fix/delivery-inventory-severity`. Its saved diagnostic explicitly says the earlier landed result was speculative, the native gate failed, and main did not advance. It remains blocked; `pr_queue` has zero rows. No active agent dispatch or automation execution was recorded in the current session.

Tracked by [THE-586](https://linear.app/blakeashley/issue/THE-586) and [THE-589](https://linear.app/blakeashley/issue/THE-589), both In Progress. `fix/delivery-inventory-severity` and `fix/integrate-outcome-safety` are **not ancestors of current main**. Do not report their changes as shipped or trigger automatic cleanup based on speculative fold results.

**5. Medium — backend resolution reports host fallback. Confirmed degradation decisions.**

Seven current-run warnings state that no container backend is available and that execution is falling back to the host without a kernel boundary. These are resolution decisions, not proof of seven successful launches; some precede failed session reattachments. The message names podman-rootful as installed but unavailable. Current pane-open evidence also includes ordinary daemon-host execution.

Evidence: `thegn.log:54199`, `54220`, and subsequent launch warnings. The global config names `bwrap`, but effective per-pane configuration and existing terminal choices can differ. These observations do not prove that bwrap is globally broken or that every pane lacks isolation. The snapshot includes a local terminal explicitly recorded with observed backend `host`.

Follow-up: verify the intended backend for the affected pane/environment and surface the effective result. If a pane requires containment, apply that pane's isolation-floor policy rather than silently accepting a host fallback. No backend or service was started or reconfigured during this audit.

**6. Medium — background work and latency exceed parts of the documented performance goals. Confirmed measurements; attribution needs profiling.**

The first frame flushed at **419 ms**, above the repository's sub-300-ms objective. Session loading reached 342 ms, configuration 344 ms, and sidebar state 364 ms. Startup Git healing completed asynchronously at 458 ms, after the first frame; [THE-78](https://linear.app/blakeashley/issue/THE-78)'s specific Git-heal ordering is improved in this run.

Across **566** current-run performance rollups:

| Metric                                                    | Observation                         |
| --------------------------------------------------------- | ----------------------------------- |
| Median event-loop idle ratio                              | 99.31%                              |
| Median render p50 bucket lower edge                       | 2.048 ms                            |
| Median render p99 bucket lower edge                       | 8.192 ms                            |
| Rollups with render p99 bucket above 16 ms                | 30 of 566                           |
| Largest render p99 bucket                                 | 32.768–65.536 ms                    |
| Largest input p99 bucket                                  | **262.144–524.288 ms**, at 16:35:16 |
| Median wakes per second                                   | 2.09                                |
| Median renders per second                                 | 0.67                                |
| Median hydration CPU per approximately 10-second interval | 487 ms                              |
| Median Git process starts per second                      | 1.62                                |
| PTY budget hits                                           | 0                                   |

The histogram reports the **lower edge** of power-of-two buckets, not exact event durations. The tail figures above are therefore ranges; medians of rollup percentiles are not whole-session percentiles. Independent review further established that normal frame timing ends at asynchronous writer submission, input timing is dispatch-to-next-submission rather than keypress-to-display, and idle accounting excludes synchronous input-handler work. See the [performance review](live-build-2026-09-13-performance-review.md) for precise coverage limits.

There are 549 rollups with zero reported PTY bytes and no recorded input latency, yet their median wake/render rates remain 2.09/0.67 per second. Hydration alone consumes roughly 4.9% of one CPU core per typical 10-second interval. Event-loop idle ratio does not include all producer-thread CPU and is not proof of zero process CPU.

For context, 11,797 retained old-run rollups had median hydration CPU 265 ms, median render-p50 bucket 0.512 ms, median render-p99 bucket 8.192 ms, and median wake rate 2.35/s. The new sample suggests more hydration cost, but differences in active workspaces, terminal geometry, load and capture length prevent a controlled regression claim. Profile session-load work, hydration, and the restored-pane input path before assigning cause. No profiler signals were sent to the live process.

**Supplemental observation through 16:47:50:** four explicit slow-frame warnings occurred at 16:41:47, 16:42:27, 16:42:37 and 16:46:07. Each had a render-p50 bucket of 16.384–32.768 ms. Three had render-p99 buckets of **131.072–262.144 ms**; the fourth was 65.536–131.072 ms. Reported render-busy ratios were 13.6–17.7%. System temperature alerts at 16:39:56 and 16:46:19 reported 90.75°C against an 85°C warning threshold. The logs do not establish that thermal throttling caused the frame stalls, or that thegn caused the system temperature. These newer observations make responsiveness an active follow-up, not merely an old-build issue. Evidence: `thegn.log:103517`, `103931`, `104058`, `106541`; captured separately in `final-performance-warnings.json`.

**7. Medium — repeat diagnostics obscure useful evidence and shorten retention. Confirmed, largely pre-existing.**

Of 3,587 current warnings, **2,526** are repeated compatibility notices for `workspaces_dir` and `workspace.cms`; **478** are unknown-extension/MIME notices. These two classes alone account for approximately **84%** of warnings. Compatibility notices are emitted on every configuration load/overlay in [config.rs:5880 and 5975](../../crates/thegn-core/src/config.rs).

[log_trace.rs:159–185](../../crates/thegn-core/src/log_trace.rs) contains a deliberate `log=error` default to suppress third-party classification noise. `just live` replaces that filter with broad debug directives, re-enabling the noise. The old daemon rotations contain **1,263,340 DEBUG records in about five seconds** on September 7, with sampled messages logging individual unhandled output bytes. This is historical; the current daemon has only 27 captured records.

Follow-up: emit compatibility diagnostics once per changed config/source, optionally migrate the two accepted spellings, and preserve the third-party log filter in the live recipe. The current 20-MiB × six-file host retention covers only about 34 hours, preventing a complete old-build log audit.

**8. Medium — config reload resets connectivity history and produces misleading recovery messages. Source defect supported by current logs.**

The current run logs **29 “network back online” messages with no corresponding offline message**. [Config::post_process](../../crates/thegn-core/src/config.rs), line 6072, calls network installation. [NetworkConfig::install](../../crates/thegn-core/src/config_network.rs), line 65, calls [install_thresholds](../../crates/thegn-core/src/connectivity.rs), line 274, which replaces the state machine with a new `ConnState` on every call. Its next successful request therefore produces another Unknown→Online edge.

Follow-up: update policy without discarding connectivity state and accumulated failure history on unchanged config reloads. The observed recovery messages alone are not evidence that the user's network actually went offline 29 times.

**9. Medium — temporary worktree and cleanup state needs reconciliation. Confirmed records; no deletion is justified by this audit alone.**

The live host repeats three missing-directory registry records **1,249 times each**, for `/tmp/thegn-audit/worktrees/queue-validation`, `config-validation-severity`, and `delivery-inventory-severity`. Git still lists those worktrees. In the audit filesystem view, `git worktree list --porcelain` reports **35 registrations, 26 prunable temporary entries**. The live logs independently confirm three missing paths; namespace differences mean the larger count should be verified from the owning host before cleanup.

There are also ten warnings that merged worktrees were retained because they have uncommitted changes, plus a CMS cleanup refusal because Git could no longer prove directory ownership. Those refusals are protective behavior, not permission to force removal. The blocked delivery worktree still has an unmerged branch tip even though its temporary directory is unavailable.

Follow-up: reconcile registry entries, Git administration records, branch reachability and outstanding work per entry. Preserve dirty/unmerged work and do not run blanket worktree pruning or branch deletion.

**10. Medium — recurring terminal teardown panic remains unresolved in the dependency path. Historical crash, not a new-run crash.**

The previous run crashed at **14:20:36**, before the new launch, in `termwiz-0.23.3/src/terminal/unix.rs:539`, calling `self.write.flush().unwrap()` during destruction after terminal I/O returned `EIO`. The independent review corrected the initial attribution to the preceding `exit_alternate_screen().unwrap()` at line 538 and reproduced the flush panic with a private PTY. All **10 retained crash reports** have the same signature; nine predate the previous build, and one belongs to `9a601c53`.

The current lockfile still selects termwiz 0.23.3, and the current host still drops its terminal after best-effort teardown. Panic restoration in [run.rs:1206](../../crates/thegn-host/src/run.rs) helps terminal recovery but does not remove the dependency's panic-capable destructor. Backtraces in retained reports are mostly unsymbolized.

Evidence: `crash/20260913T212036791-tkr4stlo3.txt` and `thegn.log:54118`. Follow-up: exercise terminal hangup/closed-output teardown in an isolated fixture and ensure destruction cannot panic. Do not classify these old acknowledged reports as crashes of `f4c1355b`.

**11. High for the next upgrade — launcher hardening is absent; the sccache override was absent in the initial snapshot and is present at follow-up.**

At audit time the working tree is clean, and [justfile:345](../../justfile) again invokes plain `cargo build` for `release-profiling`; it contains **no `RUSTC_WRAPPER=''` override**. This differs from the earlier edit in the conversation. The audit does not establish when or why the edit disappeared. The user's successful launch remains valid evidence, but the checked-in recipe does not preserve that protection for the next rebuild.

At the later subagent-review request, `git diff -- justfile` shows that the intended override **is restored as an uncommitted change**. The current working-tree recipe therefore includes the cache workaround. The earlier observation is retained only as historical evidence and must not be used to describe the current file.

The `live` recipe also still performs broad `pkill` matches, overrides the database migration executable, and rebuilds the shared release path before launching. These are existing behaviors, not changes made by this audit. [THE-613](https://linear.app/blakeashley/issue/THE-613) tracks a staged/backup-based upgrade; `fix/live-upgrade-safety` is not an ancestor of main. Its In Progress status must not be interpreted as installed protection. [THE-90](https://linear.app/blakeashley/issue/THE-90)'s completed sandbox-cache work does not make this separate dev-shell recipe reliable.

Follow-up: persist the targeted cache override, and finish/review the staged upgrade path before treating subsequent live rebuilds as safe session-preserving upgrades. No rebuild or restart was performed during this audit.

**Verified improvements and expected behavior**

- `thegn config validate` exits **0** on the user's current configuration: zero problems and two compatibility warnings. This confirms the actual built behavior for completed [THE-569](https://linear.app/blakeashley/issue/THE-569), even though its delivery-index follow-up is blocked.
- The release binary embeds `f4c1355b`, matching main. The DB schema is 68 and passes both integrity checks. No corruption or schema mismatch was found.
- The live daemon's registry heartbeat continued advancing and was 13 seconds old at one read. It reports the session-migration-fence protocol version. Current daemon logs show successful session open/attach operations and no WARN/ERROR.
- Current stderr contains only two legacy-directory migration notices. Both old and new directories exist, and the startup migration selects the new location. No new panic is present there.
- The build runs on the **stable** channel, despite being built locally with profiling. Startup explicitly clamps experimental providers/trackers; doctor also reports the other experimental channel flags disabled. Missing experimental features are expected under this channel, not evidence of a failed rebuild.
- The live Ghostty probe reports `ctrl_digits=Some(false)`. Ctrl-digit shortcuts should be treated as unverified/unavailable under this negotiation; the capture does not prove that all keyboard handling is broken.
- Approximately 525 GiB of free filesystem space was available during the check. One system-wide 94% CPU alert appears in the primary capture; two thermal alerts appear in the supplemental window. The logs do not attribute that host load to thegn.

**Issue reconciliation and recommended order**

The tracker has **505 open issues**. The audit reviewed their inventory and inspected the directly related tickets, rather than treating status labels as proof of implementation. Known blockers include THE-586/THE-589 and unshipped THE-613. Completed THE-569 is verified in the binary; THE-84/THE-85 do not cover the new lazy-source delegation defect. THE-575 remains In Progress; its named branch currently points at existing main and does not prove a build-watch fix has landed.

Recommended order: fix the lazy-source session recovery and printable-key logging first; repair exact-repository GitHub refresh; unblock delivery metadata through the proper gate; preserve the build workaround and complete upgrade safeguards; then address config/connectivity churn, performance, terminal teardown, and stale worktree administration. New issue candidates should include concrete captured evidence and focused reproduction criteria for findings 1, 2, 3, 7, 8 and 10. No tickets, comments, status changes, commits, merges or pushes were created by this audit.

**Verification limits**

Database inspection used a read-only connection and SQLite's online-backup API, followed by checks on the copy. Doctor and config validation ran against that copied state. The copy changes the daemon scope and freezes heartbeat time: doctor's “stale daemon” result is therefore **not a live-daemon diagnosis**. Likewise, its GitHub-authentication probe runs under this agent's restricted networking and is not proof that the user's host credentials are invalid. A direct read-only daemon socket probe was denied with `Operation not permitted` in the audit environment. Host process ownership and actual live-socket responses were not independently verified.

No live terminal input, signals, service changes, configuration edits, database repairs, worktree pruning or upgrades were performed. The full compiled Rust suite, GUI/e2e interactions, and controlled profiler measurements were not run. The operational findings above are based on captured live records, database checks, targeted source inspection, actual config validation, source checks, and current issue metadata.
