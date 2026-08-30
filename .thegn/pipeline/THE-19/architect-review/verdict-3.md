# THE-19 architect review — verdict 3

## REVISE

The branch was merged with current `main` first (`3400270c`) and reviewed as
the full `git diff main...HEAD`. Small mechanical corrections were applied in
`2fabf13f`:

- fixed the stale rustdoc link to `delete_groups_with_mode`;
- delivered detached hook completion through the model refresh channel before
  pulsing the terminal waker.

The semantic gaps are recorded in
[`revision-3.md`](revision-3.md):

1. issue-panel dispatch does not roll back a failed `git worktree add`;
2. daemon/control `worktrees.create` does not roll back a failed add;
3. vanished-tab reconciliation performs SQLite and filesystem I/O on the
   event loop.

The first two can leak speculative worktrees/branches and violate the shared
create lifecycle. The third violates the strict 0%-idle repository standard.

## Verification

Passed:

- core targeted nextest: 527 passed;
- host targeted nextest: 104 passed;
- `thegn-svc --test control_schema`: 1 passed;
- `just quick`;
- clippy for `thegn-core`, `thegn-host`, and `thegn-svc` with tests and
  `-D warnings`;
- rustdoc for the touched crates with private items and `-D warnings`;
- `git diff --check`;
- pre-commit treefmt on the merge and correction commits.

Unverified or unavailable:

- direct `treefmt` could not run because `shfmt` is absent from PATH; the
  pre-commit treefmt hook did pass;
- `openspec validate --all --strict` could not run because `openspec` is not
  installed; the Nix fallback was unavailable due to the Nix daemon socket;
- `test/ratchet-check.sh` is absent;
- the lane-document smoke/full-workspace, coverage, and e2e checks were not
  run, including the prohibited `just test`/`just ci` paths;
- `thegn dispatch report` is unavailable: the PATH `thegn` binary rejects the
  `dispatch` subcommand.

All `thegn` checks used a temporary `XDG_STATE_HOME`; no live state database
was opened or migrated.
