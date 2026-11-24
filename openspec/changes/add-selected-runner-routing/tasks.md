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

- [ ] 3.1 Terminal and configured-runner adapter that invokes the route.
- [ ] 3.2 Config lookup for a project's selected runner and recursion guard.
- [ ] 3.3 Argv, cwd, stdio and per-step deadline forwarding.
