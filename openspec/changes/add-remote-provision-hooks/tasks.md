# Tasks

## 1. Hook config + contract (superzej-core)

- [ ] 1.1 Add `[env.<name>.provision]`/`[env.<name>.teardown]` config (ordered
      `hooks`, `timeout_secs`) and a resolver that builds the ordered hook plan
      with task-context env (`SUPERZEJ_TASK_ID`/`_REPO`/`_BRANCH` + worktree path)
      — **unit tests**: parse + ordering, defaults, task-context env assembly,
      empty hooks = no-op.

## 2. Lifecycle integration (superzej-host / superzej-core)

- [ ] 2.1 Run provision hooks inside `Placement::ensure()` and teardown hooks in
      `teardown()`, sequentially with per-hook timeout; non-zero exit fails
      bring-up under the existing failover/halt policy — **unit tests**: a failing
      hook halts bring-up, timeout is enforced.
- [ ] 2.2 Compose with the warm pool: a claimed spare skips provision; a fresh
      task runs it; teardown respects pool-return vs destroy.

## 3. Output surfacing (superzej-host)

- [ ] 3.1 Capture hook stdout/stderr off-loop and surface provisioning status
      (status line / notification path); persist per-task results in a small table
      only if shown beyond the live run (`user_version` bump in that case).

## 4. Docs + validate

- [ ] 4.1 Document the provision/teardown hook contract, task-context env vars, and
      timeout in `config/config.toml.example` + the sandbox/remote doc section.
- [ ] 4.2 Run `just ci` (fmt-check + lint + build + test + coverage ≥95% core +
      smoke + nix-build + `openspec validate --all --strict`).
