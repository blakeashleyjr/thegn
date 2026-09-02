# THE-19 architect review verdict 2

REVISE

Revision chunk: `.thegn/pipeline/THE-19/architect-review/revision-2.md`

The required `git merge main` was run first; it returned already up to date, so
no merge commit was necessary. The full `git diff main...HEAD` was reviewed.
No small mechanical code correction was needed: `cargo fmt --all -- --check`,
`git diff --check`, and all available lint/test gates are clean.

The implementation still has semantic blockers. In brief: async sidebar,
workspace, and merge destroy workers omit provider/placement teardown; the
sidebar has no explicit force retry after a blocking pre-destroy failure;
vanished-tab reconciliation re-enters physical destruction and leaves stale
groups; session-end is fired after removal instead of at the live-session
boundary; the hook env admits inherited secret-shaped `THEGN_*` variables; the
wizard does not honor approved repo `wait=true` hooks; several create failure
paths leak speculative worktrees; and Windows hook timeout cleanup kills only
the direct child instead of using the existing Job Object seam.

Verification:

- core required filter: PASS (527)
- host required filter: PASS (104)
- `thegn-svc --test control_schema`: PASS (1)
- focused host lifecycle filter: PASS (21)
- `just quick`: PASS
- `cargo clippy -p thegn-core --tests -- -D warnings`: PASS
- `cargo clippy -p thegn-host --tests -- -D warnings`: PASS
- rustdoc with `RUSTDOCFLAGS="-D warnings"` for core and host: PASS
- `cargo fmt --all -- --check`: PASS
- `git diff --check`: PASS

Unverified/unavailable:

- `openspec validate --all --strict`: unavailable; `openspec` is not installed.
- `treefmt`: unavailable; its cache required escalation and then `taplo` was
  missing from PATH. Cargo fmt passed.
- `test/ratchet-check.sh`: not present.
- smoke/e2e, full workspace, `just test`, and `just ci`: not run per the
  request's prohibition. The smoke script remains an OpenSpec unchecked item.
- `thegn dispatch report`: unavailable; the PATH binary has no `dispatch`
  subcommand. All test/build commands used isolated XDG state paths; no live
  state DB was used.
