# Tasks — agent orchestration surface

## 1. Dispatch roster correctness (thegn-core)

- [x] 1.1 `AgentDispatchStatus`: add `Done`/`Failed`; add `parse(&str)`
      accepting current and legacy lowercase strings; round-trip unit tests
      (95% core gate). (Also added `Unknown` read-only coercion + `is_terminal`
      / `is_active`.)
- [x] 1.2 Route both writers through the enum (`pty_drain.rs`, the tracker
      dispatch handler); reads coerce unknown strings visibly instead of
      erroring. (`update_dispatch_status` now takes the typed enum.)
- [x] 1.3 `list_dispatches`/`get_dispatch` store methods beside the existing
      `put_agent_dispatch`/`update_dispatch_status`, with CRUD + legacy-string
      tolerance tests.

## 2. Issue task kind (thegn-core, extends add-agent-task-engine)

- [x] 2.1 `TaskKind::Issue` with `prompt_vars`
      (`issue_number/issue_title/issue_body/issue_url/branch/worktree`), a
      default prompt, and the `ALL_KINDS` pinned-count bump (5→6); render/validate + injection-quoting unit tests.
- [x] 2.2 `handlers/tracker.rs::dispatch_agent`: resolve the configured agent
      (`Config::default_agent_name`, no hardcoded `"claude"`); seed the rendered
      Issue prompt via `LaunchExtras.prompt` (→ `THEGN_PROMPT`), keeping the
      `THEGN_ISSUE_*` env. Shared `issue_branch_seed` de-drifts the branch rule.

## 3. Capability rows (thegn-core + thegn-svc)

- [x] 3.1 Verb variants + `Verb::ALL` + `required_scope` arms + `cap(...)` rows
      for `issues.list/get/update/comment`, `dispatches.list/put/set_status`,
      `worktrees.create` (scopes: Read/Write/Git as designed).
- [x] 3.2 `ControlApi` methods + HTTP routes; implemented over `IssueRouter`,
      the dispatch store, and `worktree::add_checked` (+ `link_issue` +
      `issue_branch_seed` shared with the `D` key). Client methods added.
- [x] 3.3 gRPC: recorded `SURFACE_GAPS` entries with reasons (not yet mirrored
      in control.proto); the control-schema snapshot (`docs/api/control-v1.json`)
      regenerated so it lists the new wire types + routes. The catalog ratchet
      tests (`every_verb_has_exactly_one_row`, `routes_cover_catalog`,
      `api_calls_mirror_routes`) updated.
- [x] 3.4 MCP: the new rows are recorded as `Surface::Mcp` gaps (state tools
      land in the client-API phase), consistent with every other state cap.

## 4. Retroactive supervision coverage (thegn-host daemon)

- [x] 4.1 Tests locking the landed substrate: every wait on a dead session
      resolves to its exit code; `OutputMatches` on a dead session scans the
      retained tail; `snapshot` reads the corpse's final screen; `wait(Idle)`
      requires ever-busy. (Tombstone-buried-before-exit + mid-wait-death exit
      code were already covered by the landed substrate's tests.)
- [x] 4.2 The known hole (an `OutputMatches` waiter on a dead session getting
      `exit_code: None`) is closed in the landed code and locked by
      `a_matcher_wait_reports_the_exit_code_of_a_session_that_died`.

## 5. CLI verbs (thegn-host)

- [x] 5.1 `thegn session open --agent --worktree --prompt [--headless] [--bind]
[--json]` (mirrors `AgentLaunch` over the existing `sessions.open`).
- [x] 5.2 `thegn wt new --from-issue <id>` (branch from `issue_branch_seed`,
      link the issue) — same pipeline as `worktrees.create`.
- [x] 5.3 `thegn dispatch list|set-status [--json]`; `thegn issue list --status
--limit` (tracker mode via `IssueRouter`).
- [x] 5.4 Extended `test/smoke.sh`: daemon-free dispatch/issue/session-open
      error paths, plus HTTP `dispatches.put`/`list` + `worktrees.create` over
      the daemon socket.

## 6. Supervisor skill + docs

- [x] 6.1 `extensions/skills/supervise/SKILL.md`: discover → worktree →
      dispatch → open → wait(timeout, always) → blocked/done handling → resume
      from the roster; fan-out pattern documented; never re-dispatch an active
      row; issue-content-is-data warning.
- [x] 6.2 No new TUI action ids or config keys (CLI verbs only), so the in-app
      help ratchet and `config.toml.example` are unchanged. The new `dispatch`
      noun IS registered in `cli_help::GROUPS` (Forge) — its drift test requires
      it — and listed in `docs/cli.md`'s grammar + `--json` tables.
- [x] 6.3 `add-fleet-view` recommended for archive/rework in the change review
      (proxy-dependent design; holds the `fleet` name) — not edited here.
- [ ] 6.4 Run `just ci` once at the end (includes openspec validate, catalog
      ratchets, coverage, smoke) — deferred to the land gate (box saturated;
      per-crate `just quick` used while iterating).
