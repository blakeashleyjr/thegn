REVISE

## Review basis

`main` was merged first in commit `f57f6879` (`Merge branch 'main' into
tg/the-29-fork-sessions`), and the implementation was reviewed as the full
`git diff main...HEAD`. The small mechanical correction and this review
material landed in `d5fb3c34` (`fix(the-29): clean MCP fork capability description`).

## Revision required

See:

- `.thegn/pipeline/THE-29/architect-review/revision-1.md`

The blocking findings are:

- forked PTYs are hard-coded to 24x80 instead of inheriting live source
  geometry, and the handoff uses the 500-line tombstone bound instead of the
  documented 2,000-line snapshot bound;
- an explicit recorded source harness can be replaced by the configured
  agent's provider during host resolution;
- the fork orchestration added a large second spawn path to `service.rs`;
- the active OpenSpec change contradicts the implementation on MCP and leaves
  delivered tasks unchecked;
- the actual daemon fork path lacks the required integration coverage.

## Gates

- Host mandatory filter: 105/105 passed.
- Service control schema: passed.
- `just quick`: passed with `RUSTC_WRAPPER=` and `XDG_RUNTIME_DIR=/tmp`.
- Touched-crate clippy with `-D warnings`: passed.
- Rustdoc for `thegn-core`, `thegn-host`, and `thegn-svc`: passed.
- Treefmt: 0 changes with `--no-cache --allow-missing-formatter`.
- Post-correction MCP state tests: 64/64 passed.
- Core mandatory filter: 526 passed; one unrelated existing
  `sandbox::tests::oci_local_secrets_go_to_env_file_not_argv` failed outside
  the THE-29 diff.

## Unverified / unavailable

- `openspec validate --all --strict` could not run because `openspec` is not on
  PATH; the active OpenSpec mismatch is therefore explicitly a revision item.
- Plain `treefmt --no-cache` could not initialize the missing `taplo` formatter;
  the allow-missing run found no formatting drift.
- `test/ratchet-check.sh` is not present.
- The lane's real-socket service test and CLI smoke were not rerun; the lane
  reports the socket test blocked by restricted socket setup and the smoke was
  not run without a scoped binary.
- No live `thegn` state DB or migration was used. The PATH `thegn` binary does
  not support `dispatch report`, so no dispatch report was filed.
- The diff-analysis knowledge graph is absent at
  `.understand-anything/knowledge-graph.json`, so no graph overlay was
  generated.
