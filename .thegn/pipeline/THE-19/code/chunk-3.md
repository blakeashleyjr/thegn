---
files:
  - docs/help/configuration.md
  - docs/help/workspaces-and-worktrees.md
  - docs/help/cli.md
  - docs/help/sandboxing.md
  - docs/help/pipeline-board.md
  - test/completion-slot-ratchet.txt
  - test/help-ratchet.txt
  - test/help-prose-ratchet.txt
  - test/help-panel-prose-ratchet.txt
  - crates/thegn-svc/tests/control_schema.rs
  - docs/api/control-v1.json
  - test/smoke.sh
overlaps: []
after: [chunk-1, chunk-2]
---

# Chunk 3 — authored docs and surface/ratchet verification

## Scope and approach

Document `[hooks]` at global, `[workspace.<slug>]`, and repo-overlay scopes;
all six events; shorthand/object entries; cwd and environment; timeout and
`wait`; ordering; legacy `sandbox.prepare`; repo trust; failure/force and
unattended semantics; session hooks; and the distinction between hooks and
per-pane `init_script`. Update CLI/worktree and sandbox help so `wt rm
--force` explains the failed-hook override. Explain that pipeline stages are
still structure-only and external supervisors reach the shared lifecycle via
`wt new` or `worktrees.create`.

Keep authored pages’ action frontmatter unchanged: existing delete/new actions
already cover the UI. Confirm the generated config-reference page derives all
new keys from `config.toml.example`.

Run the completion-slot, help action/prose/context, and control-schema tests.
The expected result is no completion/help/control snapshot mutation: `--force`
is reused, no capability/API type changes, and hook prose is in existing
pages. If implementation changes the control wire despite this design, update
`docs/api/control-v1.json` only through its sanctioned snapshot command and
include that diff in this same commit. Add a narrow smoke marker for a
successful setup/failure-visible teardown if the existing script harness can
exercise it without introducing an e2e run; do not broaden the smoke suite.

## Dependencies and overlap

No file overlap with chunks 1 or 2. Run after both because the docs must match
the final public config and host behavior. The control snapshot files are
owned here only to enforce the no-wire-change check; they should remain byte
identical unless an accidental API change is deliberately corrected.

## Tests to run

- `just quick thegn-host`
- `cargo nextest run -p thegn-host help`
- `just quick thegn-svc`
- `cargo nextest run -p thegn-svc control_wire_matches_the_committed_snapshot`
- `cargo nextest run -p thegn-core config_example`

Do not run `test/smoke.sh` as e2e for this issue, and do not run a full
workspace build, `just test`, or `just ci`.

## Done criteria

- All paths in the frontmatter are the only paths touched by this coder.
- Every new config key is described in authored help and is covered by the
  generated config-reference/key-coverage tests.
- Completion-slot, help, and control-schema ratchets pass with no unjustified
  additions; any intentional snapshot change is in this same commit with its
  sanctioned command and rationale.
- The smoke addition, if made, is narrow and does not require running e2e here.
- Scoped tests above pass.
- Commit exactly with subject: `docs(the-19): document lifecycle hooks and ratchets`
