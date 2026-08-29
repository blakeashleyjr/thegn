# THE-87 — Security / test / bug review

Branch: `tg/the-87-live-fallback-workspace`
Base: `main` (merged first in `df2eca24`)

## Verdict

PASS

Ready for the merge queue. The merge step remains `thegn integrate`.

## Review result

- Reviewed the full `git diff main...HEAD` and all THE-87 lane documents,
  including every coder `Unverified` section and the architecture follow-up.
- Found and fixed two issues during review:
  - A higher-version DB still changed journal mode and could run the
    once-per-process startup prune. `68cc844b` now opens it read-only, skips
    all open-time writes, preserves `user_version`, and verifies reads/writes
    are safe (writes are rejected).
  - Recovery trusted the first non-empty slug match. `b57dcf2a` now requires
    live tab/path corroboration, rejects conflicting roots, covers empty and
    foreign rows, separates same-basename slugs, and heals folders before
    folder lookup.
- The global action, composite action, and template action all use the one
  `new_worktree_target` resolver. An unresolved focused sidebar row refuses;
  active-tab fallback remains limited to no-sidebar-row/terminal contexts.
- The logged DB swallows remain on the hydration path only; no wake source,
  per-frame I/O, or ignored-result ratchet debt was added.

## Verification

All stateful commands used `XDG_STATE_HOME=/tmp/tmp.R2qc66zIFD`; no migration
or binary was run against the live state DB.

- `cargo nextest run -p thegn-core -E 'test(env_overlay) | test(config_example) | test(control_schema) | test(capability) | test(db)'` — 513/513 passed.
- `cargo nextest run -p thegn-host -E 'test(complete) | test(help) | test(catalog_tests) | test(platform_ratchet) | test(mq_assets) | test(render_plan)'` — 120/120 passed.
- Focused sidebar/hydration filters — 14/14 passed.
- `just quick thegn-core` — passed.
- `just quick thegn-host` — passed.
- `cargo clippy -p thegn-host --tests` — passed with no warnings.
- `git diff --check` — clean.

The lane’s warn-once diagnostics-ring assertion and full/e2e gates remain
unrun as documented by the lane; the once guard and higher-version fixture
were reviewed/tested. No e2e was run per scope.

## Frame impact

This lane changes frames: a healed live-fallback workspace can show its
registered worktree/folder rows, and an unresolved new-worktree action shows a
refusal status. Re-record the focused-sidebar snapshots:

- `test/muse/snapshots/sidebar__focused/xterm__100x30__linux.txt`
- `test/muse/snapshots/sidebar__focused/xterm__160x40__linux.txt`

No snapshot/e2e run was performed in this review lane.
