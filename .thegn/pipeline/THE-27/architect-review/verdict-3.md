REVISE

Revision chunk: `.thegn/pipeline/THE-27/architect-review/revision-3.md`

The required `git merge main` was a clean no-op because `main` is already an
ancestor of this branch. The full `git diff main...HEAD` was reviewed.

Findings requiring revision 3:

- `crates/thegn-host/src/pr_view.rs:331-343,1201-1247` duplicates the complete
  outdated/general feedback block under every expanded file, while
  `pr_view.rs:540-543,710-713` makes `p` unable to hand off an outdated/general
  row when a file is expanded. Reply and handoff use different selection models.
- `crates/thegn-host/src/diff_view.rs:149-171,480-565` counts review rows that
  are not rendered in expanded mode and adds review rows to the default
  Worktree source, where they are not rendered at all. This permits invisible
  selections and changes local Worktree behavior.
- `crates/thegn-host/src/diff_view.rs:530-543` renders top-level comments but
  omits submitted top-level reviews from the PR-review feedback block.
- `crates/thegn-host/src/pr_view.rs` grew from 1,305 lines on `main` to 1,889
  lines; review row/action/render logic remains in the modal god file despite
  the design’s explicit module-boundary requirement.

Self-fix committed:

- `ecc3fdf1 fix(the-27): avoid review glyph literals` replaces the new review
  draw-site glyph literals with capability-neutral text/ASCII fallback.

Verification:

- Passed: core land-gate filter (530 tests), host land-gate filter (104 tests),
  service `control_schema`, `just quick`, strict clippy for `thegn-core` and
  `thegn-host` test targets, rustdoc with warnings denied, cargo format check,
  git diff check, and the focused PR/diff/handoff/actions suite (42 tests).
- `treefmt` could not run: its cache path is read-only in this environment and
  the no-cache retry reports missing `shfmt`; the commit pre-hook treefmt passed.
- `openspec validate --all --strict` could not run because `openspec` is not on
  PATH. `test/ratchet-check.sh` is absent; the host ratchet tests passed in the
  required host filter.
- Not run: e2e, `just test`, `just ci`, live forge/pane/headless-agent
  integration, migration, or binary/live-state probes. No new config key,
  control route, or snapshot regeneration is present.
