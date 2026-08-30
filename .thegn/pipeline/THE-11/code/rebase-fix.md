# THE-11 — merge main and re-gate (Lead work order)

files:
  - (whatever the merge and the gate require)

## Why this exists

Row 316 reported a green `THEGN_ALLOW_HEAVY=1 just test` (7126 passed) and that
was true AT THE TIME. Since then `tg/the-27-pr-comments-in-diff` and
`tg/the-7-theme-builder-popup` landed on main, and `thegn land` now refuses this
branch: **gate red against the NEW base.** A green gate goes stale the moment
main moves; this row re-establishes it.

## Done criteria

- `git merge main` in this worktree. Resolve any conflict on the MERGED
  semantics — do not take one side wholesale just to make it compile.
- Run `THEGN_ALLOW_HEAVY=1 just test` and fix what it reports. Report the
  failing test names in your artifact even if you fix them, so the supervisor
  can see what the stale base hid.
- Keep the drawer feature and the row-316 config documentation intact; this is
  a rebase/repair row, not a redesign.
- If the gate dies before any test runs (e.g. `sccache: Operation not
  permitted`), retry once with `RUSTC_WRAPPER=` unset and report BLOCKED with
  the exact error rather than FAIL.
- Report PASS only with a green full gate against current main.
