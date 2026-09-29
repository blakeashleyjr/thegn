# Tasks — pipeline supervisor, phase 1 (policy)

## 1. Classification (thegn-core)

- [x] 1.1 `thegn_core::pipeline_validate`: `ValidationClass` (green,
      compile-error, test-failure, lint-finding, inconclusive,
      environment-error), composing `gate::classify_exit` for the
      environment/code split; per-matcher marker sets with a generic fallback;
      `digest` capped in line count and line length.
- [x] 1.2 Tests pinning the two expensive mistakes: a trailing generic error
      line is not a compile break; a compile break outranks the test noise it
      causes; an environment failure blames nothing and does not satisfy green.
- [x] 1.3 Test that no marker names a coding-agent harness (word-boundary, so
      ordinary toolchain vocabulary is not forbidden).

## 2. Approvals (thegn-core)

- [x] 2.1 `thegn_core::pipeline_approval`: `Approval`, `ApprovalState`
      (live / absent / revoked / superseded / expired / malformed), `state_for`.
- [x] 2.2 Commit binding: prefix-aware, case-insensitive, minimum prefix length
      so an empty record cannot match every tree.
- [x] 2.3 Expiry: the earlier of the record's own deadline and the configured
      TTL wins; a huge TTL clamps before the cast rather than wrapping into the
      past.

## 3. The planner (thegn-core)

- [x] 3.1 `thegn_core::pipeline_supervise`: `LaneFacts`, `SuperviseAction`
      (validate / advance / enqueue / escalate / hold), `EscalationReason`,
      `Authorization`, `plan`.
- [x] 3.2 One action per lane, in input order, including `Hold` with its reason.
- [x] 3.3 Idempotence over unchanged facts; de-duplication keyed on the lane's
      head commit, never on a row's status.
- [x] 3.4 Guards: landed lane, live-or-unknown worker, unresolvable head,
      operator-closed row, stage absent from the chart.
- [x] 3.5 `wants_primary` separates a request for judgement from a finding, so
      only the former will ring a doorbell in phase 4.

## 4. Config (thegn-core)

- [x] 4.1 `PipelineStage::validate` (names `[[tasks]]` entries) and
      `requires` (closed `Requirement` vocabulary).
- [x] 4.2 `[pipeline.supervisor]`: `enabled` (default false),
      `validate_on_exit`, `advance`, `land`, `land_requires` (default
      `["approval"]`), `approval_ttl_secs`, `max_validations`.
- [x] 4.3 `validate_pipeline` refusals: unknown task, unknown/duplicate
      requirement, requirement on a parentless stage, `validation:green` whose
      parent declares no `validate`, land gate needing green from a terminal
      stage that declares none, enabled-with-nothing-on.
- [x] 4.4 `config/config.toml.example` documents every new key.

## 5. Storage (schema v70)

- [x] 5.1 `migrate_v70`: `pipeline_validations` and `pipeline_approvals`, both
      `UNIQUE` on their commit; additive and idempotent.
- [x] 5.2 `db_pipeline_supervise`: record (upsert, counting attempts), read,
      grant (clearing any revocation), revoke, latest, list.
- [x] 5.3 `SCHEMA_VERSION` 69 → 70 and the migration wired into `Db::init`.

## 6. Read-only surface (thegn-host)

- [x] 6.1 `thegn supervise plan|status|validations`, working regardless of
      `enabled`, printing the authorizing facts alongside each action.
- [x] 6.2 Fact gathering: roster + `row_liveness` + `verify_facts` + git head +
      `merge-base --is-ancestor` + the two ledgers + the merge queue.
- [x] 6.3 CLI registration: help group, `command_intent`, completion catalog
      slots (reusing the roster's reserved sources).

## 7. Verification

- [x] 7.1 `cargo nextest run -p thegn-core -p thegn-host` green.
- [x] 7.2 `cargo clippy` clean on both crates.
- [x] 7.3 `supervise plan` read against a copy of the live 678-row roster in an
      isolated state home — landed lanes held, the genuinely open lanes
      escalated with the exact commit to review. (Two bugs found this way:
      the blank authorization and the unresolvable head.)
- [x] 7.4 `just coverage` — `thegn-core` at or above the 95% line gate.
- [x] 7.5 `openspec validate --strict`.
- [x] 7.6 `just smoke`.

## 8. Unrelated defect fixed to get the gate green

- [x] 8.1 `config_admission_tests::rejected_candidate_does_not_install_process_global_remote_policy`
      was order-dependent and failed under `just coverage`. It planted a
      sentinel via `remote_tune::set_ssh_tune`, which is **first-set-wins by
      design** — so under `cargo test`'s shared process (what `coverage` runs,
      unlike nextest's process-per-test) both the sentinel and its closing
      "restore" were silent no-ops once any other test had loaded a config, and
      the assertion turned on which test ran first. Rewritten to compare the
      global before against after, which proves the same property and does not
      care about ordering. Pre-existing; surfaced by this change adding tests to
      the same process.

## 9. Not in this phase

Execution of any kind — running validations, dispatching a stage, granting an
approval, touching the merge queue, the daemon task — plus the capability
catalog rows, the `pipeline` event frame, automation event kinds and the board
column. Phases 2 to 4.
