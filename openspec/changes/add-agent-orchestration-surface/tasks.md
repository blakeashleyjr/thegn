# Tasks — agent orchestration surface

## 1. Dispatch roster correctness (thegn-core)

- [ ] 1.1 `AgentDispatchStatus`: add `Done`/`Failed`; add `parse(&str)`
      accepting current and legacy lowercase strings; round-trip unit tests
      (95% core gate).
- [ ] 1.2 Route both writers through the enum (`pty_drain.rs:789`, the tracker
      dispatch handler); reads coerce unknown strings visibly instead of
      erroring.
- [ ] 1.3 `list_dispatches` store method beside the existing
      `put_agent_dispatch`/`update_dispatch_status`, with CRUD tests.

## 2. Issue task kind (thegn-core, extends add-agent-task-engine)

- [ ] 2.1 `TaskKind::Issue` with `prompt_vars`
      (`issue_number/issue_title/issue_body/issue_url/branch/worktree`), a
      default prompt, and the `ALL_KINDS` pinned-count bump; render/validate
      unit tests.
- [ ] 2.2 `handlers/tracker.rs::dispatch_agent`: resolve the configured agent
      (no hardcoded `"claude"`); seed the rendered Issue prompt, keeping the
      `THEGN_ISSUE_*` env.

## 3. Capability rows (thegn-core + thegn-svc)

- [ ] 3.1 Verb variants + `Verb::ALL` + `required_scope` arms + `cap(...)` rows
      for `issues.list/get/update/comment`, `dispatches.list/put/set_status`,
      `worktrees.create` (scopes: Read/Write/Git as designed).
- [ ] 3.2 `ControlApi` methods + HTTP routes; implement over `IssueRouter`,
      the dispatch store, and `worktree::add_checked` (+ `link_issue` +
      `branch_hint` derivation shared with the `D` key).
- [ ] 3.3 gRPC: mirror in `control.proto` or record `SURFACE_GAPS` entries
      with reasons; refresh the pinned control-schema snapshot; the catalog
      ratchet tests (`every_verb_has_exactly_one_row`, `routes_cover_catalog`)
      green.
- [ ] 3.4 MCP: confirm the new rows surface as scope-gated state tools on the
      write-tools branch's machinery (Write rows require the write scope).

## 4. Retroactive supervision coverage (thegn-host daemon)

- [ ] 4.1 Tests locking the landed substrate: tombstone-buried-before-exit
      ordering; `wait(Idle)` requires ever-busy; no transition lost between
      subscribe and level-probe; `OutputMatches` sees retained scrollback;
      attention → blocked → cleared by stdin; a waiter whose session dies
      mid-wait receives the exit code.
- [ ] 4.2 Fix the known hole while testing it: an `OutputMatches` waiter on a
      dead session gets `exit_code: None` though the tombstone knows it.

## 5. CLI verbs (thegn-host)

- [ ] 5.1 `thegn session open --agent --prompt --worktree [--headless]
[--bind] [--json]` (mirrors `AgentLaunch`).
- [ ] 5.2 `thegn wt new --from-issue <id>` (branch from `branch_hint`, link
      the issue) — same pipeline as `worktrees.create`.
- [ ] 5.3 `thegn dispatch list|set-status --json`; `thegn issue list
--status --limit`.
- [ ] 5.4 Extend `test/smoke.sh` to drive open → wait → dispatch list
      headlessly (isolated `XDG_STATE_HOME`).

## 6. Supervisor skill + docs

- [ ] 6.1 `extensions/skills/` supervisor skill: discover → worktree →
      dispatch → open → wait(timeout, always) → blocked/done handling → resume
      from the roster; fan-out pattern documented; never re-dispatch `Running`.
- [ ] 6.2 `docs/help/` updates for any new action ids/verbs (help ratchet) and
      `config/config.toml.example` for any new keys.
- [ ] 6.3 Recommend `add-fleet-view` for archive/rework in the change review
      (proxy-dependent design; holds the `fleet` name) — do not edit it here.
- [ ] 6.4 Run `just ci` once at the end (includes openspec validate, catalog
      ratchets, coverage, smoke).
