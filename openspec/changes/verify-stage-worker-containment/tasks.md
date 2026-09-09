# Tasks

## 1. Shared resolution and result model

- [ ] 1.1 Extract a redacted `StageWorkerProbePlan` from the production
      stage-worker resolver, covering stage agent/command identity, env provenance,
      provider homes, callback binary, inner isolation, outer backend, worktree, and
      Git mount topology.
- [ ] 1.2 Define typed pass/fail/unsafe/not-proven subresults and stable failure
      categories for worktree, Git metadata, callback, provider home, containment,
      and platform/backend support.
- [ ] 1.3 Add unit tests proving doctor and launch consume the same resolved plan
      and that secrets/credential contents are redacted.

## 2. Doctor probe

- [ ] 2.1 Add a non-billable deterministic payload that exercises the resolved
      containment without launching an agent/provider or making network calls.
- [ ] 2.2 Verify worktree writes, Git add/commit/HEAD, callback executability,
      provider-home visibility/mode, shared-config protection, and denial of one
      purpose-created outside path.
- [ ] 2.3 Render each failure category and remediation separately in doctor;
      host-only/static evidence and unsupported platforms must read `not proven`,
      never healthy.
- [ ] 2.4 Bound probe runtime/output, use isolated Git identity and temp roots,
      and clean up on every exit path.

## 3. Unsafe-pair enforcement

- [ ] 3.1 Detect an inner full-access harness paired with resolved outer backend
      `none`/uncontained during automated stage admission.
- [ ] 3.2 Put that stage on an infrastructure hold before launch and make doctor
      report the same decision, winning config layers, and remediation.
- [ ] 3.3 Add precedence/fallback tests so a requested backend that degrades to
      `none` cannot be reported or admitted as contained.

## 4. Linux/bwrap end-to-end smoke

- [ ] 4.1 Build a disposable repo plus linked worktree fixture with isolated
      fake home/provider home, explicit Git identity, and a narrow outside sentinel.
- [ ] 4.2 Run the real bwrap composer and prove `git add`, `git commit`, and
      tracked `HEAD` state succeed while the sentinel write fails.
- [ ] 4.3 Prove temp paths and probe processes are cleaned after success, failure,
      and panic; never derive destructive targets from a user home or repository.
- [ ] 4.4 Make unsupported local runs explicit skips/not-proven while the
      designated Linux containment CI lane fails if bwrap proof cannot execute.

## 5. Integration and evidence

- [ ] 5.1 Reuse bounded backend-probe plumbing with THE-90 where practical, but
      keep compiler-cache and commit-containment results independent.
- [ ] 5.2 Document doctor output, unsafe admission behavior, supported platforms,
      and remediation without exposing credential material.
- [ ] 5.3 Run focused core/host doctor, stage-resolution, sandbox, and smoke tests;
      strict OpenSpec validation; and the normal repository gate under isolated
      state/runtime directories.
