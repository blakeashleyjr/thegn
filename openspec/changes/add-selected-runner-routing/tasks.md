# Tasks — selected runner routing (contract slice)

## 1. Contract

- [x] 1.1 Add `SelectedRunner`, `RunnerDirectory`, `SelectedJobResult` and
      `SelectedCompletion` with terminal exit, cancellation, timeout, signal and
      indeterminate states.
- [x] 1.2 Add `submit_selected` refusing a missing runner, missing recipe, an
      unpinned source or a source mismatch before any runner call.
- [x] 1.3 Forward the spec deadline to the runner and reject every recipe field
      the route cannot forward.
- [x] 1.4 Separate pre-submission refusals from failures after submission so the
      readback never reports a possibly-run job as rejected.
- [x] 1.5 Read receipts back through `JobResult`, including a bounded log tail
      and its truncation flag.
- [x] 1.6 Publish `runner`, `recipe` and `Recipe` descriptions in
      `schemas/job-v0.json` and prove schema equality in the contract test.

## 2. Verification

- [x] 2.1 Contract tests cover the selected build route, refusal without
      fallback, deadline forwarding, unhonoured-field rejection,
      lost-after-submit, timeout/signal completion and truncated output.
- [ ] 2.2 Run `cargo nextest run -p thegn-job-spec` in the dev shell and record
      the result in the issue handoff.
- [ ] 2.3 Run `just ci` (includes `openspec-validate`).

## 3. Follow-up (not this change)

- [x] 3.1 Configured-runner adapter that invokes the route: a factory builds
      the Sprites or Vercel executor from the env's provider config
      (`thegn-host` `selected_runner`), and `thegn job run [--runner <env>]
[--deadline <secs>] <cargo-build|cargo-test>` snapshots the worktree,
      pins the digest, submits through `submit_selected` with Ctrl-C wired to
      `SelectedCancel`, prints the receipt and exits with the job's exit code.
      Interception of arbitrary terminal commands stays out of scope (a recipe
      is chosen by name, never parsed from a command line).
- [x] 3.2 Config lookup for the selected runner (`[jobs] runner`, validated to
      name a sprites or vercel env) and the recursion guard (the launchers set
      `THEGN_SELECTED_JOB=1` in the recipe environment; `job run` refuses when
      it is set).
- [x] 3.3 Argv, cwd, stdio and per-step deadline forwarding: argv/cwd forwarded only for
      closed recipes by design (owner may revisit); the closed `Recipe` enum avoids an
      injection surface.

## 4. Sprites executor (THE-2417)

- [x] 4.1 Execute a pinned recipe on a project-bound Sprite behind
      `SelectedRunner::submit(.., deadline)`: single non-retried exec upgrade,
      dispatch fence, `TimedOut`/`Signalled` mapping, kill of the owned process
      group and hold release at the deadline.
- [x] 4.2 Verify the checkout against the REAPI root digest, with per-attempt
      cleanup, a separate transfer budget, one project per Sprite and a
      scrubbed per-attempt environment.
- [ ] 4.3 Run `cargo nextest run -p thegn-svc --test sprites_job` and the
      `sprites::job` unit tests on the Mac builder.
- [x] 4.4 User cancel: a `CancelToken` on a running attempt issues the same
      owned-tree kill as the deadline, records `Cancelled` (distinct from
      `TimedOut`), fences outputs, releases the hold and marks cleanup `Done` on
      a confirmed kill or `Pending` otherwise.
- [x] 4.5 Pending-row reconciler: scan the ledger for unsettled attempts,
      re-issue the kill for the recorded control file and hold, and mark cleanup
      `Done` only on confirmation. It never dispatches, so a command with an
      unknown dispatch outcome is never rerun as an implicit recovery.
      Sprites: `sprites/job.rs` `reconcile_pending_at`; Vercel: `vercel/job.rs`
      `reconcile_pending_at`.
- [x] 4.6 Reattach by session handle: follow the stream while the provider still
      holds it, otherwise reconcile to `Indeterminate`; never redispatch. (`sprites/job.rs` `reattach`, scheduled by
      `selected_runner.rs` `reattach_orphans`.)
- [ ] 4.9 Route wiring: `submit` (the `SelectedRunner` trait) takes a
      `CancelToken` so a user cancel before or during dispatch reaches the
      runner and reads back as `Cancelled` on the route (today a pre-dispatch
      cancel surfaces as `RunnerUnavailable`); schedule `reconcile_pending` and
      `reattach` on startup. 4.4-4.6 are runner-level only until this lands.
      Partly done: `SelectedRunner::submit` takes a std-only `SelectedCancel`
      (hooks bridge it to the Sprites `CancelToken`); a cancel before dispatch
      reads back as `Cancelled` on both executors and mid-run on both (Vercel
      kills the command and stops the sandbox through the deadline path). The
      executors are now built from provider config (task 3.1), and the daemon
      runs one background `reconcile_pending` pass at start for the configured
      `[jobs] runner` (Vercel gained a `reconcile_pending` wrapper over its
      per-attempt `reconcile`; the pass scans only ledgers with unsettled rows,
      so an idle ledger resolves no secret). The same pass then calls Sprites
      `reattach` for `Running` rows with a session handle and no live owner
      lease (live leases are never touched), off the event loop. Still open:
      reconciling ledgers of envs other than the configured runner.
- [x] 4.10 Owner lease on non-terminal rows (pid, boot id from
      `/proc/sys/kernel/random/boot_id`, process start time, heartbeat) so
      recovery skips a live owner. Shared `OwnerLease` in `sprites::job`, used
      by both executors: the owner refreshes the heartbeat every 15 s by
      compare-and-swap and fences when it loses; reconcile, reattach and
      cancel treat a live lease (fresh heartbeat, or same boot and live pid on
      Linux) as not stale. Recovery writes are already compare-and-swap.
      The heartbeat checks ownership (stored row non-terminal and carrying this
      run's lease) and rewrites only the lease, never the whole row; reattach
      leaves a live-leased row alone before any provider call; a heartbeat more
      than 5 s in the future is not fresh. Known limits: `OwnerLease` and
      `AttemptRecord` use `deny_unknown_fields`, so an older binary sharing the
      ledger cannot read an in-flight (non-terminal) row written by a newer one
      (terminal rows clear the lease); and a row whose owning task panicked in a
      still-running process stays unrecovered until that process exits.
- [ ] 4.7 Run the new cancel, reconcile and reattach tests in
      `cargo nextest run -p thegn-svc --test sprites_job` on the Mac builder.
- [ ] 4.8 Not in this slice: the credential broker and a budgeted live run. The
      CLI verb landed with task 3.1; it has not yet run against a real provider.
- [x] 4.11 Follow-up: Vercel `reconcile_pending_at` reports a compare-and-swap loss as
      `Cleaned`/`StillPending`, never `ReconcileOutcome::Superseded` (Sprites reports it);
      only the logged `settled` count is affected. `settle_outcome` now returns the
      superseded flag and the pass maps it to `Superseded`, matching Sprites; a
      fixture test forces another recovery's write between the scan and the
      re-read.
- [x] 3.3 Per-project runner binding (`[project.<slug>.jobs] runner`, resolved by
      `Config::repo_job_runner`) and the `thegn job route [--json]` inspector.
      Interception of stock agent commands (shell adapter) remains out of scope.

## 5. Automatic route (THE-2416 slice B)

- [x] 5.1 `[project.<slug>.jobs] route = "off"|"auto"` (opt-in per project, no
      global key), resolved by `Config::repo_job_route`, validated, and shown
      as `automatic` by `thegn job route`.
- [x] 5.2 Matcher for direct, `&&`-compound and piped lines over the closed
      recipes, with fixture tests; unmodelled shell syntax is reported as outside
      coverage (`thegn job match`).
- [x] 5.3 `thegn job shim` / `shim-install`: PATH wrapper, per-call decision,
      real-tool passthrough without the shim directory, recursion guard,
      fail-closed on refusal, explicit `THEGN_JOB_LOCAL=1` escape.
- [x] 5.4 A host agent pane in a `route = "auto"` project with a bound runner
      gets the managed shim directory (`$XDG_STATE_HOME/thegn/job-shim`, written
      idempotently by the `shim-install` code) first on `PATH`, plus
      `THEGN_JOB_SHIM_DIR`. No effect when `route` is not `auto`.
- [x] 5.5 Bounded admission wait: a routed job waits up to `[jobs]
admission_timeout_secs` (default 30, 0 = no wait) for an unavailable runner,
      then fails closed with a message naming `THEGN_JOB_LOCAL=1`.
- [ ] 5.6 Stock-harness probe on the Mac builder: record the pinned harness,
      OS and recipe that passed, and compare the explicit and automatic
      receipts. An explicit `job run` alone does not qualify the route.
- [ ] 5.7 Run `cargo nextest run -p thegn-host job_shim` and `-p thegn-core
config_jobs repo_job_route` on the Mac builder.
