## Why

A worktree removal or release could report success while a provider sandbox was still running, and quitting the UI had no confirmation.

## What Changes

- A destroy is done only when a complete provider inventory no longer lists the sandbox.
- `wt rm` fails closed; `land --release-sandbox` keeps the location without proof; closing a worktree says its sandbox is kept.
- Keep-awake reports a failed hold and never releases one it did not get.
- Doctor shows a "Remote live readiness" block.
- New key `[ui] confirm_quit` (default true).
- No schema change, no new verb or flag.

## Impact

- Specs: provider-seams.
- Docs: configuration, environments, quick-reference, cli.
