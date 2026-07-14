# Tasks

## 1. Core datamodel (thegn-core)

- [ ] 1.1 Extend `Issue` in place as `WorkItem` (`pub use Issue as WorkItem`,
      `IssueStatus as WorkStatus`) with serde-defaulted `kind: WorkItemKind`
      (named to avoid the `thegn_core::work::WorkKind` collision), `parent_id`,
      `cycle_id`, `estimate: Option<f64>` (drops `Eq`, keeps `PartialEq`),
      `custom_fields`, `status_raw`; add `Project`, `Cycle { scope_id, .. }`
      (team scope), `Transition`, `RelationKind` in
      `crates/thegn-core/src/issue.rs` — **unit tests** for status mapping,
      `<instance>:<key>` parse, and serde round-trip incl. old-row
      forward-compat (95% gate on thegn-core).
- [ ] 1.2 Add the new `[issues]` keys (`agent_write`, `auto_in_progress`,
      `move_on_pr_open`, `in_review_state`, `meta_ttl_secs`; extend
      `move_on_merge` to local `thegn land` merges), the
      `[issues.linear.accounts.<name>]` tables (`api_key` secrets-ref,
      default `team`/`project`), `[workspace.<slug>] tracker = { provider,
      account, team, project }` + `issues_agent_write`, and the `.thegn.toml`
      `[issues.linear]` overlay `account`/`team`/`project` (`team_id` deprecated
      alias) in `crates/thegn-core/src/config.rs`; pure resolution via
      `Config::repo_issues_scoped(root, slug)` (repo overlay → workspace binding
      → account defaults → legacy `team_id`); secrets via `expand_env_ref` —
      **unit tests** for layering, precedence, and `env:`/`file:` token
      expansion (95% gate).
- [ ] 1.3 Migration in `crates/thegn-core/src/db.rs`: bump `SCHEMA_VERSION`
      43 → 44, add `tracker_meta (provider, scope, kind, json, fetched_at,
      PRIMARY KEY(provider,scope,kind))` (~900s TTL) and `issue_detail_cache
      (issue_id PRIMARY KEY, json, fetched_at)` (60s TTL), activate the dormant
      `issue_projects` table (no new `project_cache`), and clear
      `issue_cache`/`issue_projects` rows (pure caches); new
      `issue_relations.kind` values — **unit tests** for migration and cache
      read/write/TTL (95% gate on thegn-core).
- [ ] 1.4 Pure transition planning in `crates/thegn-core/src/issue_flow.rs`
      (auto-in-progress / move-on-pr-open / move-on-merge incl. the fold-actor
      `thegn land` drain) — **unit tests** for each planned transition
      (95% gate).

## 2. Provider trait + router (thegn-svc)

- [ ] 2.1 Generalize `IssueBackend` into `TrackerBackend` + `TrackerCaps` and the
      static-dispatch `TrackerClient` enum in `crates/thegn-svc/src/issue/mod.rs`,
      mirroring `ci::CiProvider`/`CiCaps`/`CiClient`; the shim collapses to
      `pub use TrackerBackend as IssueBackend` so existing callers compile —
      unit tests for cap-gated `IssueError::Unsupported` degradation.
- [ ] 2.2 Implement every capability on **Linear as the reference provider**
      (all caps true except `custom_fields`): `list_projects`/`project_items`/
      `list_cycles(scope_id)` (team-scoped), `available_transitions`/
      `transition`, `add_comment`; multi-account instances (`"linear"` /
      `"linear@<account>"`), `<instance>:<key>` `split_once(':')` dispatch, and
      `backend_for_id` exact-instance → first-of-kind fallback in `IssueRouter`
      (reuse `list_per_provider` for cache diffing) — router fan-out, instance
      routing, and Linear parse unit tests.
- [ ] 2.3 Keep Jira and GitHub Issues on their current methods with honest,
      mostly-false `caps()` (new methods inherit the `IssueError::Unsupported`
      defaults) — per-provider cap unit tests. Completing Jira and adding GitHub
      Projects v2 / GitLab providers are follow-up changes.
- [ ] 2.4 `thegn tracker login linear [--account NAME]`: interactive key paste,
      validate against the API, store at
      `$XDG_STATE_HOME/thegn/accounts/linear/<name>/api_key` (0600); key
      resolution config-ref → stored login → skip instance with warning —
      smoke-test the CLI parse; unit-test the pure resolution order.

## 3. Host wiring (thegn-host)

- [ ] 3.1 Warm `issue_projects` + `tracker_meta` inside
      `spawn_issue_cache_refresh` on the existing `RefreshKind::Issues` path;
      single `TerminalWaker` pulse, off-loop on `spawn_blocking` — refresh
      plumbing in the new `crates/thegn-host/src/hydrate_tracker.rs` (pinned
      `hydrate.rs` may only shrink) — keep the Issues-cadence invariant test
      green.
- [ ] 3.2 Render the Project→Cycle→WorkItem→subtask tree and the grouped
      ByStatus view in `Section::Issues` (kanban board deferred to the follow-up
      change `add-tracker-board-view`); gate transition/comment/assignee
      write-back actions on `caps()` — rendering in
      `crates/thegn-host/src/panel/sections/issues/{mod,list,detail,tiers,md}.rs`,
      key handling in `crates/thegn-host/src/handlers/tracker.rs` (pinned
      `run.rs`/`chrome.rs` may only shrink).
- [ ] 3.3 Generalize worktree-from-item + status-on-merge across providers
      (incl. local `thegn land` merges via the fold-actor drain),
      `auto_in_progress` transition on worktree-create, `move_on_pr_open` →
      `in_review_state`, and `worktree set --comment` write-through to the
      linked item via `issue_links`, driven by `issue_flow` plans.
- [ ] 3.4 `HouseTracker` MCP tools: trait in `crates/thegn-core/src/mcp/mod.rs`
      (HouseGit/HouseForge/HouseMerge pattern); read tools `tracker_my_issue`/
      `tracker_list`/`tracker_get`/`tracker_search` serve the DB cache with a
      staleness line (`refresh=true` fetches live); write tools
      `tracker_update_status`/`tracker_comment`/`tracker_assign` gated by
      `[issues] agent_write` + `[workspace.<slug>] issues_agent_write` —
      omitted from `tools/list` AND refused at dispatch when disabled; omitted
      `id` resolves the worktree's linked issue via `issue_links`; writes are
      live then synchronously refresh affected cache rows — unit tests for
      gating and id resolution.
- [ ] 3.5 Document the new config keys (`agent_write`, `auto_in_progress`,
      `move_on_pr_open`, `in_review_state`, `meta_ttl_secs`, accounts tables,
      workspace tracker binding) in `config/config.toml.example`.

## 4. Render invariants

- [ ] 4.1 Confirm tracker refresh marks **chrome** dirty and yields `Full` (idle
      wake still `Skip`); assert via `render_plan::plan` unit tests — no tick/timeout
      added, 0% idle preserved.

## 5. Validate

- [ ] 5.1 Run `just ci` (includes `openspec-validate`).
