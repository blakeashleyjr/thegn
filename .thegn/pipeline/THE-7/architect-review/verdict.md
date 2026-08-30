REVISE

Revision chunk:

- `.thegn/pipeline/THE-7/architect-review/revision-1.md`

Review commits:

- `6617b3b5` — merged current `main` into `tg/the-7-theme-builder-popup` as
  required before review.
- No source correction commit was made; the remaining findings are semantic
  and are fully described in the revision chunk.

Verification:

- Passed: required host scoped nextest gate (104/104), service control-schema
  snapshot (1/1), `just quick`, touched-crate clippy with `-D warnings`,
  rustdoc with `-D warnings`, cargo fmt check, and git diff check.
- Passed: focused THE-7 core theme tests (8/8) and host builder/store tests
  (4/4).
- Failed unrelated gate: required core scoped nextest had one pre-existing
  `sandbox::tests::oci_local_secrets_go_to_env_file_not_argv` failure in
  `crates/thegn-core/src/sandbox_tests.rs`; THE-7 does not touch that code.

Unverified/environment-limited:

- `treefmt` could not run because `taplo` is unavailable; cargo formatting
  check passed.
- `openspec validate --all --strict` could not run because `openspec` is not
  on PATH; OpenSpec scope synchronization is itself a revision finding.
- `bash test/ratchet-check.sh` was not available in this checkout.
- E2E/visual snapshots were not run, and no live thegn process or live state
  database was exercised.
