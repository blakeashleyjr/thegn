# CI provider deadlines: a per-operation audit

Status: audit (THE-538). No code lands with this note. It records, for every
CI provider operation, the deadline that bounds each phase of the call, the
end-to-end worst case, and the gaps that remain. THE-538 stays open; the gap
list below names its next slices.
Evidence is cited against `c005a66c2`.

THE-538's remaining item (2026-11-15) is "end-to-end deadline on every wait and
join". This note answers it per provider, per phase, and per host caller.

## 1. The two subprocess runners

Every `gh` and `glab` call goes through one of two svc runners, and both
delegate to `thegn_proc` on Unix. So the phase bounds are the same for every
CLI-backed operation. Only the timeout differs.

| Phase                       | `run_cli` (JSON, mutations)                                                                                                                                                                                                 | `run_bounded_log` (logs)                                                                                       |
| --------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------- |
| Entry point                 | `run_cli` ci.rs:443 -> `run_cli_within` :455 -> `gh_run::run_blocking` gh_run.rs:102                                                                                                                                        | `run_bounded_log` ci.rs:825 -> `run_bounded_log_within` :839 -> `gh_run::run_blocking_streaming` gh_run.rs:175 |
| Spawn                       | `Command::spawn` (fork/exec), no deadline. Bounded in practice by the kernel.                                                                                                                                               | same                                                                                                           |
| stdout/stderr read          | Unix: in-thread `poll` (thegn-proc unix.rs:403) under the run timeout                                                                                                                                                       | same; stdout chunks go to `LogCollector::feed` on the calling thread                                           |
| Leader wait                 | `CI_CLI_TIMEOUT` = 30 s, ci.rs:60 (`Limits::silent(timeout)`, gh_run.rs:102-119)                                                                                                                                            | `CI_LOG_TIMEOUT` = 60 s, ci.rs:61                                                                              |
| Survivors after leader exit | `quiesce_after_leader: Some(ZERO)` gh_run.rs:112: group killed at once                                                                                                                                                      | same, gh_run.rs:185                                                                                            |
| Group kill                  | `term_grace` ZERO (SIGKILL at once), then `kill_settle` 250 ms for the group to empty and 250 ms for the leader (thegn-proc lib.rs:89)                                                                                      | same                                                                                                           |
| Reader join                 | No reader threads on Unix. Open pipes get `READER_GRACE` = 500 ms (gh_run.rs:100) and are then closed                                                                                                                       | same                                                                                                           |
| Poll slack                  | one `BACKOFF_CAP` = 250 ms (thegn-proc lib.rs:26)                                                                                                                                                                           | same                                                                                                           |
| Per-call wall               | timeout + 2 x 250 ms + 500 ms + 250 ms = **31.25 s** (formula at thegn-proc lib.rs:285)                                                                                                                                     | **61.25 s**                                                                                                    |
| Cancellation                | `cancel: &AtomicBool` polled by `thegn_proc`. It reaches the child only when the client was built with `provider_*_cancellable` (ci.rs:196, :206)                                                                           | same                                                                                                           |
| JSON parse                  | `decode_provider_json` ci.rs:1332 after the `bounded_json` preflight ci.rs:1303 with `PROVIDER_JSON_LIMITS` ci.rs:67 (8 MiB, depth 32, 262,144 items). Size-bounded, so CPU time is linear in at most 8 MiB. No wall clock. | no JSON; `LogCollector` ci.rs:875                                                                              |

Windows (c): `run_blocking` calls `thegn_proc::run`, which falls back to
`fallback::run_consuming_streams` (thegn-proc fallback.rs:172) with
`Containment::DirectChildOnly` (fallback.rs:251, :305). The leader is killed,
not its tree, and the reader threads (fallback.rs:51) are detached, not joined.
The log path does not use thegn_proc on Windows: `run_bounded_log_within`
branches on `bounded_group_termination_supported()` (plugin/proc.rs:1296,
`cfg!(unix)`) to `run_fallback_bounded` ci.rs:545. That fallback has its own
deadline, plus `FALLBACK_READER_GRACE` = 500 ms (ci.rs:79) applied up to three
times (post-exit wait :679, `fallback_terminate` :757, reader drain :694). Its
readers are dropped, not joined (ci.rs:714-715). Windows per-call wall is about
timeout + 1.5 s, but a grandchild that holds a pipe leaks a reader thread.

The process-wide concurrency bound `MAX_CONCURRENT` = 4 (gh_run.rs:42) applies
only to the async `gh_run::run` (gh_run.rs:69). Neither CI runner takes it.

## 2. Per-operation table

Phase columns cite section 1. "Pre" is the origin lookup that runs before the
provider subprocess: `origin_url` ci.rs:433 -> `GitLoc::origin_url`
(thegn-core remote.rs:1144). For a local worktree the lookup is memoised, but on a miss
it runs an unbounded `git remote get-url` `.output()` (remote.rs:1149). For a
remote or provider worktree it is `git_out` with `GITLOC_READ_DEADLINE` = 60 s
(remote.rs:24, :1070) and a fresh, never-set cancel flag (remote.rs:1083). The
lookup runs once per client at selection (`provider_and_context_for_cancellable`
ci.rs:206), and again per operation only for an unbound client from
`client_for_system` (ci.rs:268).

| Provider / op                                             | Pre                                                                                                       | Runner                                                                                                                                                | Leader wait                                                                                                                                                            | Kill + reader                                                  | Parse                                                                  | Per-op worst                                                                                                  |
| --------------------------------------------------------- | --------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------- | ---------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------- |
| GitHub `runs` (ci.rs:1676)                                | origin                                                                                                    | `run_cli`                                                                                                                                             | 30 s                                                                                                                                                                   | 1.25 s                                                         | `parse_gh_runs_with_discarded` ci.rs:1811, preflight                   | 31.25 s                                                                                                       |
| GitHub `run_detail` (:1705)                               | origin                                                                                                    | `run_cli`                                                                                                                                             | 30 s                                                                                                                                                                   | 1.25 s                                                         | `decode_provider_json`                                                 | 31.25 s                                                                                                       |
| GitHub `logs` (:1745)                                     | origin                                                                                                    | `run_bounded_log`                                                                                                                                     | 60 s                                                                                                                                                                   | 1.25 s                                                         | collector                                                              | 61.25 s                                                                                                       |
| GitHub `workflows` (:1755)                                | origin                                                                                                    | `run_cli`                                                                                                                                             | 30 s                                                                                                                                                                   | 1.25 s                                                         | `parse_gh_workflows` :1935                                             | 31.25 s                                                                                                       |
| GitHub `trigger` / `rerun` / `cancel` (:1763/:1775/:1786) | origin                                                                                                    | `run_cli`                                                                                                                                             | 30 s                                                                                                                                                                   | 1.25 s                                                         | none                                                                   | 31.25 s                                                                                                       |
| GitLab `runs` (:2152)                                     | origin                                                                                                    | `run_cli`                                                                                                                                             | 30 s                                                                                                                                                                   | 1.25 s                                                         | preflight                                                              | 31.25 s                                                                                                       |
| GitLab `run_detail` (:2172)                               | origin                                                                                                    | header `run_cli`, then job pages `run_blocking` with `gitlab_page_timeout` (:2326) clipped to `GITLAB_DETAIL_DEADLINE` = 60 s (:64, started at :2179) | 60 s total                                                                                                                                                             | 1.25 s (last call only; earlier overruns consume the deadline) | per page, `GITLAB_JOBS_MAX_PAGES` 50 (:57)                             | **61.25 s**                                                                                                   |
| GitLab `logs` (:2234)                                     | origin                                                                                                    | `run_bounded_log`                                                                                                                                     | 60 s                                                                                                                                                                   | 1.25 s                                                         | collector                                                              | 61.25 s                                                                                                       |
| GitLab `workflows` (:2246)                                | none                                                                                                      | no subprocess                                                                                                                                         | 0                                                                                                                                                                      | 0                                                              | none                                                                   | 0                                                                                                             |
| GitLab `trigger` / `rerun` / `cancel` (:2252/:2272/:2294) | origin                                                                                                    | `run_cli`                                                                                                                                             | 30 s                                                                                                                                                                   | 1.25 s                                                         | none                                                                   | 31.25 s                                                                                                       |
| Drone / Woodpecker / Jenkins / Argo                       | `client_for_system_with_config_and_cancel` ci.rs:288 returns `None` (:299-303). No client, no subprocess. |                                                                                                                                                       |                                                                                                                                                                        |                                                                |                                                                        | n/a                                                                                                           |
| Plugin, every op (plugin/ci_provider.rs:300-386)          | none                                                                                                      | `ProviderBridge::call` (plugin/provider.rs:208) on the resident runtime, then `mpsc::Receiver::recv()` (ci_provider.rs:66) on the blocking caller     | `timeout_at(deadline, rx)` provider.rs:260, deadline = `PluginSpec::run_timeout()` clamped 1..=`MAX_PLUGIN_TIMEOUT_SECS` = 3600 s (thegn-core plugin_api.rs:409, :474) | no subprocess of its own                                       | `parse_runs`/`parse_detail`/`bounded_log` ci_provider.rs:153/:179/:220 | up to 3600 s. The `recv()` itself has no timeout; it returns when the runtime task sends or drops the sender. |

The plugin adapter is dormant. `PluginCiProvider::new` has no production
caller (grep: only ci_provider.rs tests), and `CiSystem::Plugin` maps to no
client (ci.rs:303).

## 3. Host callers: end-to-end worst case against the caller's wall

"Remote pre" is the 60 s remote origin lookup above; a local worktree's
prelude is effectively zero (memo hit, no-spawn branch read).

### 3a. Background refresh (`ci_refresh.rs`)

- Launch: `crate::sched::spawn_bg` (ci_refresh.rs:917). No join. The worker
  loop chains queued requests (:930), each with a fresh `CallBudget`.
- Wall: `MAX_LOG_INGEST_WALL` = 120 s (:26), with `MAX_LOG_INGEST_CALLS` = 32
  (:22) and `MAX_LOG_INGEST_BYTES` = 8 MiB (:24). The deadline is checked only
  in `CallBudget::take` (:148) and `exhausted` (:140), before a call starts.
  Nothing clips the call itself.
- Per worktree: origin (selection, :1110), remote branch
  `loc.git_text(["rev-parse", ...])` (:1115, 60 s, cancel-blind), `runs`
  (:1129), then per failed run `run_detail` (:1345) and up to 16 failed-job
  `logs` (:1381, :1426).
- Worst case, local worktree: a 61.25 s call admitted just before 120 s ends at
  about **181 s** (1.5x the wall).
- Worst case, remote background worktree: `sweep_one_background` checks
  `exhausted()` and then calls `refresh_ci_cache_for`, whose remote prelude
  (origin 60 s + rev-parse 60 s) runs before `take()` refuses. That is about
  **240 s** (2x the wall), spent on reads whose result is then discarded.
- Quit: `quit_cancel::wait_or_detach` waits 2 s (run.rs:1306, :1342) on the
  coordinator `Condvar` (ci_refresh.rs:267). Provider subprocesses see the
  cancel within one poll plus 1 s of kill and grace, but the remote git reads
  above do not, so quit detaches.

### 3b. Daemon `ci.runs` (daemon/service.rs:2208)

- Phases: `spawn_blocking` for branch + context (:2226; remote: `git_out`
  60 s + origin 60 s), `with_db` (:846), then `spawn_blocking` for selection +
  `runs` (:2290-2296; remote origin 60 s again + 31.25 s).
- Host join: `spawn_blocking(..).await` with no timeout. It is bounded only
  through the inner calls.
- Worst case: local 31.25 s; remote about **211 s**. The caller has no wall.
- The client is built with `provider_and_context_for` (:2292), which is not
  cancellable, so a dropped control request leaves the subprocess running to
  its full bound. There is no flight and no concurrency cap.

### 3c. Daemon `ci.logs` (daemon/service.rs:2357)

- Admission: `CI_LOG_FLIGHTS.enter(key, MAX_LOG_INGEST_WALL)` (:2384), a
  120 s `tokio::time::timeout` on the per-key mutex (:127).
- Miss: one `spawn_blocking` (:2469) runs selection (:2472), `run_detail`
  (:2477) and `logs` (:2485) in sequence, with no shared deadline. The
  `.await` has no timeout.
- Holder worst case: GitHub 31.25 + 61.25 = 92.5 s; GitLab 61.25 + 61.25 =
  **122.5 s**; add 60 s for a remote worktree.
- Waiter worst case: 120 s wait + its own fetch = about **242.5 s** local
  GitLab, **302.5 s** remote.
- The holder's own worst case (122.5 s local GitLab, 182.5 s remote) is longer
  than the 120 s waiter wall. A waiter can therefore get `CiLogFlightBusy` while
  the holder is healthy.
- b134 lane 1 (THE-544) adds a global permit wait of
  `CI_LOG_FETCH_PERMIT_WAIT` = 30 s ahead of the fetch. When it lands, the
  waiter worst case grows by 30 s.

### 3d. Interactive run-detail drill (`actions.rs`)

- Launch: `spawn_ci_detail_worker` -> `tokio::task::spawn_blocking`
  (actions.rs:567), fire-and-forget. The result goes through the unbounded
  `refresh_tx` (never blocks). One fetch per `CI_DETAIL_FLIGHT` (:535,
  `MAX_RUNNING_FETCHES` = 1, ci_flight.rs:46).
- Phases: selection with the **non-cancellable** `provider_and_context_for`
  (:677), `run_detail` (:682), then up to 16 failed-job `logs` (:732, :759). The
  flight token's cancel is checked only between jobs (:733).
- Worst case: GitHub 31.25 + 16 x 61.25 = **1011 s**; GitLab 61.25 + 980 =
  **1041 s** (about 17 minutes). There is no caller wall. Every queued drill
  waits behind it.
- Quit does not cancel this flight: `cancel_coordinator_for_shutdown`
  (ci_refresh.rs:252) covers the refresh coordinator and mutations only.

### 3e. Interactive mutations (`actions.rs:455`)

- `spawn_blocking` (:466), `begin_ci_mutation` (registered with the refresh
  coordinator, so quit cancels it), `provider_for_cancellable` (:478), one
  `rerun` or `cancel` call. Worst case 31.25 s (+60 s remote origin). Bounded
  and cancellable.

### 3f. CLI (`cmd/ci.rs`)

- One-shot process. Each verb makes one or two provider calls at the per-op
  bounds above (`log` makes `run_detail` + `logs`: up to 122.5 s for GitLab).
  Mutations use `cancellable_client` (cmd/ci.rs:137), so Ctrl-C reaches the
  child. The read verbs use `provider_and_context_for` (:245, :465) and stop
  only when the process dies.

### 3g. `ci_autofix.rs`

- Makes no provider call. It reads cached log entries only (ci_flight.rs:45).
  Its agent handoff is bounded by `[pr_queue] agent_timeout_secs`
  (ci_autofix.rs:661), which is outside this audit.

## 4. Gaps and next slices

Each slice is one lane: one file set, one change, named tests.

1. **The drill has no wall, and its provider calls ignore its cancel**
   (actions.rs:677, :732, :759). Slice: build the client with
   `provider_and_context_for_cancellable(.., request.token.cancel)`, and give
   `run_ci_detail_request` a `CI_DETAIL_WALL` (reuse `MAX_LOG_INGEST_WALL`)
   that is checked before each `logs` call. A cut drill delivers what it has
   and notes the cut. Tests (actions.rs, with a fake `CiProvider` like
   `FakeCi` ci_refresh.rs:1521): `ci_detail_drill_stops_fetching_logs_after_wall`,
   `ci_detail_cancel_reaches_the_provider_call`.
2. **The refresh wall is checked only before a call** (ci*refresh.rs:148,
   :1338, :1422). Slice: in svc ci.rs, let a client carry an optional operation
   deadline and clip each runner timeout to
   `min(CI*\*\_TIMEOUT, remaining)`, as `gitlab_page_timeout`(ci.rs:2326) already
does. The refresh passes its`CallBudget`deadline. Tests:`run_cli_timeout_is_clipped_to_the_operation_deadline`(svc ci.rs),`refresh_call_admitted_near_the_wall_ends_by_the_wall` (ci_refresh.rs).
3. **Remote git reads in the CI path are cancel-blind** (`git_out` /
   `git_text` build a fresh `AtomicBool`, remote.rs:1083, :1130; reached from
   ci_refresh.rs:1115, ci.rs:433, daemon/service.rs:2228-2232). They also run
   before `take()`, so a refresh can spend 120 s past its wall and the 2 s quit
   settle detaches. Slice: `GitLoc::git_text_cancellable(args, cancel)` in
   thegn-core, used by the refresh branch read and by `origin_url` on the
   cancellable selection path. Also check `budget.exhausted()` before the
   remote prelude, not after. Tests: `remote_branch_read_observes_refresh_cancel`
   (core, fake ssh), `refresh_skips_remote_prelude_when_wall_is_spent`.
4. **`ci.logs` flight wait is shorter than the holder's worst case**, and its
   `run_detail` + `logs` pair shares no deadline (daemon/service.rs:2384,
   :2477, :2485). Slice: one operation deadline across both calls (gap 2's
   mechanism), sized so that holder worst case <= the flight wait, with a
   compile-time assertion. Test: `ci_logs_fetch_worst_case_fits_the_flight_wait`.
   Sequence it after lane 1 (same file).
5. **Daemon provider calls cannot be cancelled by a dropped request**
   (daemon/service.rs:2292, :2472). Slice: a drop guard that sets an
   `AtomicBool`, as `gh_run::run` does (gh_run.rs:80-86), passed to
   `provider_and_context_for_cancellable`. Tests:
   `ci_runs_dropped_request_cancels_the_provider`,
   `ci_logs_dropped_request_releases_flight_after_cancel`.
6. **`ci.runs` has no flight and no concurrency cap.** Each call can hold 8 MiB
   of stdout plus its JSON tree, and `gh_run::MAX_CONCURRENT` (gh_run.rs:42)
   covers only the async runner. Slice: a `ci.runs` semaphore like lane 1's
   (daemon/service.rs), or a single-flight per (worktree, branch). Test:
   `ci_runs_provider_fetch_concurrency_is_capped`.
7. **The local origin lookup is unbounded** on a memo miss (remote.rs:1149,
   `.output()`). Local git, memoised, low risk. Slice: route it through
   `git_run` with `GITLOC_READ_DEADLINE`. Test: `local_origin_url_is_deadline_bounded`.
8. **The plugin CI adapter waits on an unbounded `recv()`** (ci_provider.rs:66)
   and inherits the plugin's run timeout (up to 3600 s). It is dormant today.
   Slice, due when the adapter is wired: `recv_timeout(bridge timeout + grace)`
   and a CI-specific ceiling (`CI_LOG_TIMEOUT` for `logs`, `CI_CLI_TIMEOUT`
   otherwise) instead of the plugin's own timeout. Test:
   `plugin_ci_call_returns_when_the_runtime_task_never_replies`.
9. **Quit does not cancel the drill flight** (ci_refresh.rs:252 only covers the
   refresh coordinator). Slice: register `CI_DETAIL_FLIGHT`'s running token in
   `quit_cancel::signal_all` (quit_cancel.rs). Test:
   `quit_signal_cancels_the_ci_detail_flight`.
10. **(c) Windows containment.** `run_cli` uses thegn_proc's
    `DirectChildOnly` fallback with detached readers (thegn-proc
    fallback.rs:51, :251). The log fallback drops its readers (ci.rs:714-715).
    `fallback::wait` (fallback.rs:127) is an unbounded `child.wait()` on the
    rare path where the leader is neither reaped nor marked `Survivors`.
    Slice: THE-274 Job Objects. Test (c):
    `windows_ci_cli_kills_a_grandchild_holding_stdout`.

Suggested order: 1 and 9 (UI drill; actions.rs, quit_cancel.rs), then 2 (svc
mechanism), then 4 and 5 (daemon, after lane 1), then 3, then 6. 7 and 8 are
opportunistic, and 10 belongs to THE-274.

## 5. Peak memory per operation

- **JSON ops (`run_cli`)**: stdout is `HeadOrFail` capped at
  `CI_CLI_MAX_STDOUT` = 8 MiB (ci.rs:65) and fails above the cap. stderr is a
  64 KiB tail (`CI_CLI_MAX_STDERR` :66). The serde tree is built only after the
  `PROVIDER_JSON_LIMITS` preflight (:67), so item count and depth are capped.
  Per-field retention is capped by `MAX_PROVIDER_FIELD_BYTES` = 4096 (:74).
- **GitLab `run_detail`**: all job pages together are capped by
  `GITLAB_JOBS_MAX_BYTES` = 4 MiB (ci.rs:59). Each page's `max_stdout` is the
  remaining budget (:2211-2214), and rows are capped at
  `GITLAB_JOBS_MAX_ROWS` = 5,000 (:58).
- **Logs (`run_bounded_log`)**: one `LogCollector` holds at most
  `LOG_COLLECTOR_PEAK_BYTES` = 2 x `PROVIDER_LOG_MAX_BYTES` + 11 bytes, about
  2 MiB (ci.rs:54, b133), between reads, plus one line's redaction working set.
  The text it returns is at most `PROVIDER_LOG_MAX_BYTES` = 1 MiB /
  `PROVIDER_LOG_MAX_LINES` = 2,000 (:34-35). stderr is a 64 KiB head
  (gh_run.rs:188).
- **Host log callers**: refresh and drill each run one fetch per flight
  (`MAX_RUNNING_FETCHES` = 1, ci_flight.rs:46), so each holds at most one
  collector. The refresh also charges `MAX_LOG_INGEST_BYTES` = 8 MiB per
  request after each fetch (ci_refresh.rs:24, :1442). Daemon `ci.logs` is
  single-flight per key only, so the number of fetches is unbounded at
  `c005a66c2`. b134 lane 1 (THE-544) caps it at `MAX_CONCURRENT_CI_LOG_FETCHES`
  = 2, which makes the daemon ceiling 2 x `LOG_COLLECTOR_PEAK_BYTES`.
- **Plugin**: one reply frame is at most `MAX_LINE_BYTES` = 1 MiB (plugin
  proc.rs:24), and a session queues at most `MAX_QUEUED_BYTES` = 2 MiB
  (plugin session.rs:19). `bounded_log` (ci_provider.rs:220) decodes the whole
  reply before capping, so the peak is about the frame plus its decoded copy.
- **`ci.runs`**: 8 MiB stdout plus the tree per call, with no concurrency cap
  (gap 6).
