# THE-19 revision 2 completion

## Delivered

- Finding 1: routed TUI, workspace, merge, and CLI destruction through the shared transaction: `session_end`/`pre_destroy`, selected-environment runtime teardown, physical removal, then `post_destroy`; cache cleanup no longer repeats runtime teardown.
- Finding 2: added an explicit `delete anyway (ignore blocking hooks)` choice to the existing sidebar confirmation surfaces. Normal deletion remains `User` mode; the new choice is `Force` mode and does not add a public action id.
- Finding 3: changed vanished-tab reconciliation to loop-side pruning and cache reconciliation only. It does not run hooks, spawn destroy workers, or remove physical worktrees.
- Finding 4: moved live `session_end` scheduling ahead of destroy hooks/removal, using the warn-only unattended policy, and removed the post-removal fallback scheduling.
- Finding 5: added a hook-specific inherited-environment firewall. Credential-shaped variables and all inherited `THEGN_*` variables are removed before the five `HookContext` values are added. Coverage includes `THEGN_INBOX_SECRET`, `THEGN_API_KEY`, agent-socket, and `GH_TOKEN` shapes.
- Finding 6: passed the wizard’s active worker DB into `schedule_post_create`; an approved repository `post_create` hook with `wait = true` now completes before `Done`.
- Finding 7: routed wizard, headless `wt`, and issue-dispatch post-add failures through force cleanup, preserving the primary error and appending cleanup failure details when necessary.
- Finding 8: switched hook timeout cleanup to the existing grouped-process handle, including the Windows Job Object implementation, with a timeout regression test.

## Tests and checks

- `cargo nextest run -p thegn-host ...`: 5 targeted tests passed, including lifecycle, force-menu, hook environment, grouped hook cleanup, and wizard approval/wait behavior.
- `cargo clippy -p thegn-host --tests -- -D warnings`: passed.
- `just quick thegn-host`: passed.
- Pre-commit hooks, including repository `treefmt`, passed for both revision commits.
- `git diff --check` and Rust formatting passed.

## Disputed

None.

## Unverified

- Full-workspace CI, coverage, and e2e were intentionally not run per the revision dev-loop policy.
- Direct `treefmt` invocation reported that `shfmt` was unavailable in PATH; the repository pre-commit treefmt hook nevertheless passed on both commits.
