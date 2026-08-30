# THE-19 — worktree lifecycle hooks

## Decision

Add a typed, config-driven lifecycle policy in `thegn-core` and one host-side
runner/orchestration seam in `thegn-host`. The policy resolves hook entries;
the host executes them. No shell/process/filesystem code is added to core.

Hooks are available at three existing config layers and accumulate in this
order:

```text
global config → [workspace.<slug>] → repo .thegn.{toml,yaml,yml,json}
```

Within one event, entries retain declaration order. The supported events are
`pre_create`, `post_create`, `pre_destroy`, `post_destroy`, `session_start`,
and `session_end`.

The user-facing shape is an array of command strings, with an object form for
execution policy:

```toml
[hooks]
pre_create = ["./.thegn/pre-create.sh"]
post_create = [
  { command = "pnpm install --frozen-lockfile", wait = false,
    timeout_secs = 120, on_failure = "warn" },
]
pre_destroy = ["docker compose down"]
post_destroy = []
session_start = []
session_end = []
```

The object form is normalized by core to the same `HookSpec` as the string
form. `wait` is meaningful only for `post_create`; it controls whether the
first pane is held until that entry/event completes. The default is `false`.
`timeout_secs` defaults to 120. `on_failure` defaults according to the event:
`pre_create`/user `pre_destroy` block, while the other events warn. Invalid
values are config errors, not silently ignored commands.

The legacy `[sandbox].prepare` list is retained as a compatibility alias. It
is normalized as the first global `post_create` entries (and the repo
`[sandbox].prepare` list as the first repo `post_create` entries), then uses
the new runner, timeout, logs, notifications, and failure semantics. It is no
longer a separate fire-and-forget mechanism. `[sandbox].init_script` remains a
per-pane script inside the sandbox and is not a lifecycle hook.

## Current-code verification and pruning of the OpenSpec draft

The OpenSpec change at
`openspec/changes/add-worktree-lifecycle-hooks/` was read as a draft. The
following claims were re-checked against this branch:

| Draft claim / requirement                             | Current code                                                                                                                                                                                                                                                                                               | Decision                                                                                                                                                  |
| ----------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Existing prepare is lifecycle-aware and accountable   | `SandboxConfig.prepare` is defined in `crates/thegn-core/src/config.rs:3838-3884`; `wizard.rs:1315-1329` calls `sandbox::run_prepare`; `sandbox.rs:1814-1840` starts detached threads and discards status                                                                                                  | Keep the compatibility alias, replace the runner, and surface failures.                                                                                   |
| The wizard is the only create path                    | UI creation goes through `handlers/wizard.rs:14-79` and `wizard.rs:1039-1067`, but CLI creation is `cmd/wt.rs:161-290`, issue dispatch directly calls `wt::add_checked` at `handlers/tracker.rs:219-335`, and the daemon implements `worktrees.create` at `daemon/service.rs:1314-1390`                    | Add one shared host lifecycle service and route all four paths through it.                                                                                |
| Pipeline-spawned worktrees are an internal thegn path | `config.toml.example:1531-1545` says pipeline stages are validated/displayed only; a supervising agent advances them.                                                                                                                                                                                      | Do not add pipeline-specific hooks. External supervisors use `thegn wt new` or the existing `worktrees.create` API, both of which use the shared service. |
| All destruction is one path                           | Sidebar deletion calls `run::delete_groups` from `handlers/worktree_delete.rs:264-314`; workspace deletion uses `handlers/workspace_remove.rs:41-67`; CLI removal is `cmd/wt.rs:493-627`; merge cleanup calls `merge_lifecycle::remove_landed` at `merge_lifecycle.rs:152-186` and `merge_sweep.rs:99-124` | Wire every physical-removal path. Keep `reconcile_removed_tabs` (`merge_lifecycle.rs:189-237`) as tab reconciliation only; it must not double-run hooks.  |
| Sandbox preparation is already off-loop               | The wizard worker is `spawn_blocking` (`wizard.rs:970-986`), and deletion has background threads, but `run_prepare` is in core and detached; `delete_groups` also prunes UI state before its worker finishes (`run.rs:1889-1942`)                                                                          | Preserve off-loop behavior, but move all hook process work to host workers and make deletion stateful enough to honor a user `pre_destroy` veto.          |
| A new capability/catalog/control operation is needed  | Hooks are internal side effects of existing worktree/session operations. `WorktreeCreateReq` already exists (`thegn-svc/src/control/mod.rs:447-474`) and its route is already cataloged.                                                                                                                   | No new capability row, API verb, or control-schema type. The existing daemon implementation must call the shared seam.                                    |
| `initializeCommand` is an existing thegn setting      | This branch has `init_script`, not that setting (`config.toml.example:2409-2414`).                                                                                                                                                                                                                         | Remove that draft compatibility claim.                                                                                                                    |

Already-satisfied architecture constraints are the existing off-loop wizard,
delete, merge-sweep, and notification-channel patterns. They are not reasons
to add another event-loop polling loop. The new work is the missing shared
ordering, failure state, and host runner.

## Core policy

Add `crates/thegn-core/src/hooks.rs` as a sibling module rather than growing
`config.rs` or `run.rs` into another god file. It owns:

- `HookEvent`, `HookScope`, `HookFailure`, `HookSpec`, and the normalized
  `ResolvedHooks` model;
- accumulation and ordering across global/workspace/repo scopes;
- legacy `sandbox.prepare` insertion;
- event-specific failure policy, including the `force` and `unattended`
  execution modes;
- the pure context-to-environment key/value projection for `THEGN_EVENT`,
  `THEGN_REPO_ROOT`, `THEGN_WORKTREE`, `THEGN_BRANCH`, and
  `THEGN_WORKSPACE`;
- canonical repo-hook request data for trust-on-first-use.

The resolver must never run a command, read the process environment, touch the
filesystem, or depend on a vendor. It receives already-resolved config,
approval state, and context, and returns executable entries plus pending trust
requests. Unit tests cover ordering, empty-command filtering, defaults,
failure-policy bounds, canonicalization, legacy prepare placement, and the
secret-free environment projection.

Repo overlay hooks are classified in the existing
`config_resolve::{GatedRequest, Approvals}` machinery (`config_resolve.rs:141-194`
and `351-376`). Use one canonical request per event, keyed as
`hooks.<event>`, containing the normalized event list. An unapproved request is
returned as pending and contributes no executable entries. Approval never
changes the repo source to a blocking failure policy: repo entries are always
warn-only, even after approval. This matches the existing repo-overlay
degrade-at-the-edge rule and prevents a cloned repository from vetoing local
operations.

Add `hooks: HooksConfig` to `Config`, `WorkspaceConfig`, and the typed repo
overlay. The repo overlay remains additive and trust-gated; it does not become
a full-config override. Do not add environment overrides for command arrays or
per-entry tables: they are structured lists, not safe scalar deployment knobs.
Pin `hooks.pre_create`, `hooks.post_create`, `hooks.pre_destroy`,
`hooks.post_destroy`, `hooks.session_start`, and `hooks.session_end` in
`test/env-overlay-ratchet.txt` with the reason that command lists are
structured policy and intentionally have no `THEGN_*` scalar override. This is
the required env-overlay ratchet decision, not an omission.

## Host runner contract

Add `crates/thegn-host/src/hook_run.rs` and keep process execution there. Add a
separate `worktree_lifecycle.rs` for orchestration so neither `run.rs` nor the
runner becomes a new god file.

For every entry, the runner:

1. builds `vec!["sh", "-lc", command]` and passes it through
   `thegn_core::sandbox_cpucap::wrap_background_argv`;
2. starts it off the event loop with null stdin and piped stdout/stderr;
3. clears the inherited environment and installs the curated base from
   `thegn_core::util::filter_host_env` plus the five `THEGN_*` context values;
   it does not use `env_passthrough`, `host_env_allow_extra`, or the full
   process environment, so `GH_TOKEN`, `*_KEY`, `*_SECRET`, and agent sockets
   do not leak into repo-authored commands;
4. uses the event cwd contract: repo root for `pre_create`, worktree for
   `post_create`/`pre_destroy`/session events, and repo root for
   `post_destroy`; and
5. captures output into a per-worktree state log, enforces the per-entry
   timeout, kills the process group on timeout, and returns an explicit
   success/failure/timeout result.

The CPU wrapper is mandatory even when no cap is configured. Its documented
contract at `sandbox_cpucap.rs:610-633` is fail-safe and requires callers to be
off-loop. Hook failure is never converted to `Ok(())` or discarded. A host
worker reports completion through the existing refresh channel and pulses the
`TerminalWaker`; failure goes through `notify::record` (the durable inbox and
toast funnel at `notify.rs:284-330`) and the operation status. The caller may
also write a concise `thegn_core::msg` line for CLI/headless surfaces.

## Lifecycle sequencing and failure behavior

Creation is one shared transaction around the existing git/DB operations:

```text
resolve policy/trust → pre_create → git worktree add → register/link
→ existing built-in provisioning where that path performs it
→ post_create (async unless wait=true) → first pane / completion
```

`pre_create` failure means no `git worktree add` and no registry row. The
wizard and issue dispatch must leave the UI/DB unchanged. In CLI mode, the
process waits for the configured lifecycle job before returning so a short-lived
CLI cannot orphan its worker; `post_create` still does not block the UI loop.
The daemon returns its existing `WorktreeInfo` response after scheduling the
default asynchronous post-create job and reports completion/failure through
its existing event/notification plumbing. A `wait=true` entry is a host-side
completion gate for the first pane, not a synchronous event-loop wait.

The wizard keeps the existing ordering in `wizard.rs:1213-1329`: register,
sandbox preparation, and direnv warming remain in their current path, then
post-create is scheduled before the `Done`/pane-dependent completion. CLI and
control creation currently do not eagerly provision a sandbox (`cmd/wt.rs:68-72`
documents this); their post-create hook runs after registration and any
provisioning that actually occurred. This avoids silently changing lazy
provisioning while giving all creation surfaces the same hook contract.

Destruction is:

```text
resolve policy/trust → pre_destroy → provider/placement teardown
→ git worktree remove + purge → post_destroy from repo root → DB/UI reconcile
```

For CLI `wt rm`, a failed blocking hook leaves the worktree and returns a
message naming `--force`; `--force` is the existing confirmation flag and
skips failed blocking hooks. Approved repo hooks remain warn-only. For the
sidebar, the background delete job must retain the group until pre-destroy
passes, then apply the existing `delete_groups` UI/DB prune on a refresh event.
Its existing confirmation flow supplies the explicit force/delete-anyway path;
do not invent a new bindable action id. A failed job leaves the group visible
and offers retry/force in the existing delete confirmation surface.

Workspace removal (`workspace_remove.rs:41-118`) has two meanings. The
keep-files arm only forgets the workspace and must not run destroy hooks. The
destructive arm is an explicit bulk delete: run the same per-worktree teardown
jobs off-loop, with the existing destructive confirmation treated as the
operation’s force authorization, and only then purge/reconcile each path. A
failed hook is reported with the path; other worktrees continue, and failed
paths are not falsely removed from the source of truth.

Merge-lifecycle reclaim and expiry sweep are unattended. They run
`pre_destroy` in warn-and-continue mode, remove only after the existing clean
guard, and run `post_destroy` after a successful removal. A hook cannot wedge
the merge queue. Internal wizard rollback/cancel is cleanup with force
semantics: report hook failures but never allow cleanup to leak a speculative
worktree.

`session_start` is scheduled once when the first pane for a worktree session is
about to spawn; it never delays pane creation. `session_end` is scheduled when
the last pane exits or a tab closes; it never delays close. Track the
start/end latch in host runtime state keyed by worktree path and session
identity, not in SQLite (the DB is a cache and this is an ephemeral session
edge). `init_script` remains per-pane and is not replaced by `session_start`.

## Call-site map

The host chunk must route these exact paths through `worktree_lifecycle`:

- wizard creation and rollback: `wizard.rs:1039-1067`, `1103-1110`,
  `1180-1280`, `1315-1355`;
- CLI `wt new`, `--from-issue`, and batched project creation:
  `cmd/wt.rs:161-469`;
- issue-panel `D` dispatch: `handlers/tracker.rs:219-352`;
- daemon/control `worktrees.create`: `daemon/service.rs:1314-1390`;
- sidebar deletion: `handlers/worktree_delete.rs:264-314` and
  `run.rs:1815-1955`;
- destructive workspace deletion: `handlers/workspace_remove.rs:41-118`;
- CLI removal: `cmd/wt.rs:493-627`;
- automatic merge removal: `merge_lifecycle.rs:152-186` and
  `merge_sweep.rs:99-124`.

The pipeline configuration itself is not a call site. It must remain a
structure-only roster as documented in `config.toml.example:1531-1545`.

## Ratchets and verification

The config example is the source for both the config-reference help page and
the key-coverage test (`crates/thegn-core/tests/config_example.rs`). Document
every new hook field, its shorthand/object forms, defaults, scope, cwd,
timeout, wait, failure, trust, and legacy prepare behavior in
`config/config.toml.example` and authored help pages.

The implementation must run these gates in the same commits that change the
surface:

- env-overlay coverage plus its shrink-only pin update;
- completion-slot ratchet: no new value-taking CLI argument (reuse `--force`);
- control-schema snapshot: unchanged because `WorktreeCreateReq`, routes, and
  catalog do not change;
- help action/prose/context ratchets: unchanged action vocabulary, with new
  prose in existing pages and generated config-reference coverage;
- unit tests for core policy and host runner/lifecycle transitions, including
  timeout, process-group cleanup, secret filtering, repo trust pending, and
  force/unattended failure modes.

Do not run `just test`, `just ci`, a full-workspace compile, or e2e while
implementing this issue. Each chunk has scoped `just quick <crate>` and
`cargo nextest run -p <crate> <filter>` commands.

## OpenSpec disposition

The proposal/design/spec scenarios for ordering, failures, trust, timeout,
environment, resource slice, and event-driven completion are retained. The
following draft material is cut because it contradicts this branch: a
pipeline-internal spawn path; the claim that issue dispatch reuses the wizard;
`initializeCommand`; a new capability row; blocking unattended deletion; and
core-owned process execution. The final implementation should update or
archive the OpenSpec change only as part of the normal project workflow; these
architect artifacts are the authoritative implementation handoff for THE-19.
