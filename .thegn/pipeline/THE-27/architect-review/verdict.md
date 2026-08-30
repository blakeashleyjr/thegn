REVISE

Revision chunk: `.thegn/pipeline/THE-27/architect-review/revision-1.md`

The full `git diff main...HEAD` was reviewed after merging `main` in merge
commit `32815173`. The core/cache implementation and the host integration
compile and pass the required local gates, but the selected-thread handoff,
headless confirmation, full inline-thread rendering, and structural/source
mode pairing have semantic gaps that must be corrected before landing.

Self-fix committed:

- `535089a2 fix(the-27): satisfy clippy in review test`

Verification:

- Passed: required thegn-core filtered nextest (530 tests), thegn-host
  filtered nextest (104 tests), thegn-svc `control_schema` snapshot, `just
quick`, clippy with `-D warnings` for thegn-core and thegn-host, cargo doc
  with warnings denied for both touched crates, and `cargo fmt --check`.
- Passed pre-commit treefmt during the merge and self-fix commits. Direct
  `treefmt` could not initialize because `taplo` is unavailable in the
  environment.
- Not run: `openspec validate --all --strict` because `openspec` is not on
  PATH; `test/ratchet-check.sh` is absent. No e2e, `just test`, or `just ci`
  was run. No live forge, pane, or headless-agent integration was exercised.
- The understand-diff graph overlay was unavailable because
  `.understand-anything/knowledge-graph.json` is absent.
