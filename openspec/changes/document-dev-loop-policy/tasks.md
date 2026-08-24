# Tasks

## 1. Reconcile guard ↔ justfile

- [ ] 1.1 Diff `test/heavy-guard.sh`'s recipe list against the justfile's
      current full-workspace recipes; add any heavy recipe that has appeared
      since (candidate: `bench-idle`) and drop any that no longer exists, so
      the spec'd list is accurate. No behaviour change beyond the list.

## 2. Spec

- [ ] 2.1 The `architecture-gates` delta in this change is the deliverable;
      confirm its described behaviours (refusal text pointer, opt-in
      pass-through, fail-open, quoted-mention immunity) match the script
      as reconciled in 1.1.

## 3. Validation

- [ ] 3.1 Run `just ci` once, when the change is complete (includes
      `openspec validate --all --strict`). This is itself the policy's
      run-once final gate.
