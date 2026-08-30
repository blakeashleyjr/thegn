# THE-11 architect verdict 2

REVISE

Revision chunk:

- `.thegn/pipeline/THE-11/architect-review/revision-2.md`

The branch is not ready to land because global drawer state currently uses the
persisted drawer cache and is restored on startup, contradicting the accepted
process-local-global design and synced OpenSpec. The revision chunk contains
the concrete code locations, required behavior, and regression-test shape.

Small correction applied and committed:

- `48e12e84 fix(the-11): preserve visible drawer during pooled exit`

This fixes hidden pooled-pane exits incorrectly clearing focus and geometry,
and makes cold picker/cycle transitions retain pending drawer focus.

Validation:

- `thegn-core` focused nextest command: failed in the pre-existing
  `sandbox::tests::oci_local_secrets_go_to_env_file_not_argv` test because a
  GitHub token appeared in OCI argv; 325 tests passed before cancellation.
- `thegn-host` mandatory focused nextest: 105/105 passed.
- `thegn-svc --test control_schema`: 1/1 passed.
- `just quick`: passed.
- touched-crate clippy with `-D warnings`: passed.
- treefmt: passed, no drift.
- `openspec validate --all --strict`: 170/170 passed.
- rustdoc for touched crates with private items and warnings denied: passed.
- `test/ratchet-check.sh` is not present.
- Additional THE-11 focused host tests: 18/18 passed; core drawer config
  tests: 7/7 passed.

Unverified by design/documentation: the lane's deferred snapshots and e2e
coverage were not run. No live-state migration or built binary was run.
