REVISE

Revision chunk: `.thegn/pipeline/THE-27/architect-review/revision-2.md`

The full `git diff main...HEAD` was reviewed after the branch's existing merge
commit `32815173` brought `main` into the branch. A new merge was a no-op
(`main` is already an ancestor of `HEAD`); the environment's linked-worktree
metadata is read-only, so Git's no-op merge attempt could not update
`ORIG_HEAD`.

Findings requiring the revision chunk:

- `crates/thegn-host/src/actions.rs:1004-1039` opens `DiffView` with only the
  snapshot already in `FrameModel` and never delivers a review snapshot that
  appears after the modal opens. A cold-start Changes view therefore cannot
  switch to PR review after background hydration.
- `crates/thegn-host/src/review_rows.rs:18-39`, together with
  `crates/thegn-host/src/pr_view.rs:296-299,1127-1174`, paints outdated/general
  Files feedback outside the indexed row model. Those rows cannot be selected,
  handed off, replied to, or reached by Files navigation.
- `crates/thegn-host/src/hydrate.rs:3581` stores `pr.head_ref_name`, while
  `crates/thegn-host/src/hydrate.rs:2930-2933` and
  `crates/thegn-host/src/actions.rs:733-736` validate against the local
  `panel.branch`. Complete snapshots can consequently be rejected as stale
  when local and remote head names differ.

No self-fix commit was made because all three findings are semantic feature
gaps rather than mechanical corrections.

Verification:

- Passed: filtered thegn-host land-gate tests (104), thegn-svc
  `control_schema`, `just quick`, clippy with `-D warnings` for thegn-core and
  thegn-host, rustdoc with warnings denied for both touched crates, focused
  PR/diff/handoff tests (17), and `git diff --check`.
- Failed for an unrelated baseline test: thegn-core filtered gate's
  `sandbox::tests::oci_local_secrets_go_to_env_file_not_argv` reports a secret
  on OCI argv; no THE-27 file touches that sandbox code.
- Not available: direct `treefmt` cannot initialize because `taplo` is absent;
  `openspec validate --all --strict` cannot run because `openspec` is absent;
  `test/ratchet-check.sh` is absent; service clippy hung after compilation and
  was stopped. No e2e, `just test`, or `just ci` was run, and no live forge,
  pane, headless-agent dispatch, migration, or live-state binary invocation
  was exercised.
- The requested `understand-diff` skill and knowledge-graph overlay were
  unavailable in this environment.
