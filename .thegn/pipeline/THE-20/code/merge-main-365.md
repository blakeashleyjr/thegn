# THE-20 — merge main and re-gate (Lead work order)

## Why this exists

`thegn land` refused this branch. Base drift: sibling branches landed on main
while this one was in review. Reported conflicts / failure:

    conflicts with main: catalog.rs, config.rs, config_tests.rs, config_tests_coverage.rs, config_validate.rs, control.rs, lib.rs, docs/cli.md, docs/help/configuration.md

This is the sixth branch in this drain to hit it. It is base drift, not a
defect in the work.

## Done criteria

- `git merge main`, then resolve by **keeping BOTH sides** wherever the clash
  is two independent additions to a registry, module list, control projection,
  dispatch arm, sidebar view or help page. The branches are unrelated, so
  almost nothing should be genuinely in tension. Dropping the other side
  silently deletes landed work — inspect every hunk.
- Expect COMPILE breaks as well as conflicts: landed branches add
  ActionSpec/catalog entries that this branch must supply companions for
  (row 352 hit four E0063s this way). Fix them.
- After resolving, confirm nothing landed was lost: main's tip content for each
  conflicted file must still be present.
- **Run the full gate** — `RUSTC_WRAPPER= THEGN_ALLOW_HEAVY=1 just test`.
  This work order OVERRIDES the stage prompt's "test minimally" default: this
  is a queue-boundary row, which is exactly where the dev-loop policy puts the
  heavy gate. Report the failing test names even if you fix them.
- Preserve all prior review fixes on this branch.
- Report PASS only with a green full gate against current main.
