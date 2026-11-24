# Tasks — Vercel selected runner (first slice)

## 1. Executor

- [x] 1.1 Add `vercel::job` with the `SelectedRunner` implementation, the
      launcher and the receipt.
- [x] 1.2 Add the attempt ledger: reserve under a lock, pin the team and
      project, bound spend by an envelope.
- [x] 1.3 Create the sandbox with an admitted vCPU ceiling and a bounded
      lifetime through `VercelProvider::create_named`.
- [x] 1.4 Materialize the snapshot, read every file back and compare the root
      digest to the pinned source.
- [x] 1.5 Persist the sandbox and command handles before trusting success.
- [x] 1.6 Enforce the deadline: kill the command, stop and delete the sandbox,
      fence outputs.
- [x] 1.7 Reconcile and cancel recorded attempts after checking the recorded
      team and project.
- [x] 1.8 Report incomplete cleanup as pending for the reaper.

## 2. Verification

- [x] 2.1 Fixture tests: success with exit and digest, nonzero exit, deadline,
      ambiguous start, identity mismatch before any create, recovery identity,
      token custody.
- [ ] 2.2 Route wiring and configuration of the runner choice.
- [ ] 2.3 A live run on a Vercel account.
- [ ] 2.4 Job-sandbox usage and cost through the usage tracker.
