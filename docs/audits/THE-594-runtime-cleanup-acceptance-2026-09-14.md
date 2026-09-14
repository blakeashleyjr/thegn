# THE-594 automatic cleanup resource acceptance

The conservative production cleanup policy was present, but the required regression for active projection/provider-sync resources had not called the real automatic cleanup entry point. This follow-up adds that test without changing production behavior.

`merge_cleanup_runtime_tests` creates private Git worktrees and exact landed queue rows, then places data-only projection/provider-sync entries into the actual agent registries. Projection, provider sync, and both together each produce the specific resource refusal from `remove_landed_with_config`. Queue and full cached row observations, refs, tracked bytes and resource registrations remain unchanged. Configured pre/post destroy marker hooks do not run.

A cfg(test), path-scoped observer counts and refuses entry into explicit `teardown_runtime` before ambient configuration or resource resolution. The assertions prove zero entries into that boundary on the current synchronous cleanup call stack. They do not instrument every provider command or claim coverage for future work moved to a different thread.

After fixture custody is released, the same landed selection passes through actual cleanup: pre/post hooks write their receipts, the private worktree is removed, the parent repository and branch remain, and a branch cleanup queue hold is retained. The explicit teardown observer remains zero. This positive control prevents an unrelated dirty/queue/identity refusal from making the hostile cases vacuously pass.

Registry custody restores previous entries on assertion unwind, verified with a nested private registration. The existing environment-selection fixture now checks both workspace and worktree changes before the persisted-session case. No registry fixture starts a projection, sync, provider, mount, VPN, SSH session or checkpoint. All fixture seams compile only under cfg(test); no timer or runtime configuration behavior changes.

Source checkpoint: `ceb4b5da`. Formatting and diff checks pass. Independent source review accepted the scoped boundary at ceb4b5da. Actual host cleanup gate passes50/50, including both new runtime fixtures and the strengthened workspace/worktree selection case. Receipt: `/tmp/thegn-rolling-cleanup-20260914-results.json`; log: `/tmp/thegn-rolling-cleanup-20260914.log`. The separate full configured workspace gate remains open due to THE-635, a logging failure; this scoped cleanup pass does not waive that delivery gate.
