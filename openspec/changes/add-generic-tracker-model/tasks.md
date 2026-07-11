# Tasks

## 1. Core datamodel (thegn-core)

- [ ] 1.1 Add `WorkItem`, `WorkKind`, `WorkStatus`, `Project`, `Cycle`,
      `Transition`, and `RelationKind` in `crates/thegn-core/src/issue.rs`;
      `IssueStatus`→`WorkStatus` `From`, `status_raw` field, serde defaults for
      forward-compat — **unit tests** for status mapping, `provider:key` parse, and
      serde round-trip (95% gate on thegn-core).
- [ ] 1.2 Extend `config_enum!` `IssueProviderKind` with `GithubProjects`/`Gitlab`
      and add the matching `[issues.<provider>]` sub-tables + `auto_in_progress`
      toggle in `crates/thegn-core/src/config.rs`; secrets via `expand_env_ref` —
      **unit tests** for config layering and `env:` token expansion (95% gate).
- [ ] 1.3 Add `project_cache` table + migration in
      `crates/thegn-core/src/db.rs`; bump `SCHEMA_VERSION` 21 → 22; new
      `issue_relations.kind` values — **unit tests** for migration and cache
      read/write (95% gate on thegn-core).

## 2. Provider trait + router (thegn-svc)

- [ ] 2.1 Generalize `IssueBackend` into `TrackerBackend` + `TrackerCaps` and the
      static-dispatch `TrackerClient` enum in `crates/thegn-svc/src/issue/mod.rs`,
      mirroring `ci::CiProvider`/`CiCaps`/`CiClient`; keep an `IssueBackend` shim for
      existing callers — unit tests for cap-gated `Unsupported` degradation.
- [ ] 2.2 Add `list_projects`/`project_items`/`list_cycles`,
      `available_transitions`/`transition`, and `add_comment` to providers; wire
      `RouterInner::{GithubProjects, Gitlab}` and `provider:key` dispatch in
      `IssueRouter` (reuse `list_per_provider` for cache diffing) — router fan-out
      and routing unit tests.
- [ ] 2.3 Complete the Jira provider and add GitHub Projects v2 + GitLab providers
      with honest `caps()` — per-provider cap and parse unit tests.

## 3. Host wiring (thegn-host)

- [ ] 3.1 Warm `project_cache` inside `spawn_issue_cache_refresh` on the existing
      `RefreshKind::Issues` path; single `TerminalWaker` pulse, off-loop on
      `spawn_blocking` (`crates/thegn-host/src/hydrate.rs`) — keep the
      Issues-cadence invariant test green.
- [ ] 3.2 Render the Project→Cycle→WorkItem→subtask tree and kanban board (gated
      on `caps().boards`) in `Section::Issues`; gate transition/comment/assignee
      write-back actions on `caps()` (`crates/thegn-host/src/panel/mod.rs`).
- [ ] 3.3 Generalize worktree-from-item + status-on-merge across providers,
      `auto_in_progress` transition on worktree-create, and `worktree set --comment`
      write-through to the linked item via `issue_links`.

## 4. Render invariants

- [ ] 4.1 Confirm tracker refresh marks **chrome** dirty and yields `Full` (idle
      wake still `Skip`); assert via `render_plan::plan` unit tests — no tick/timeout
      added, 0% idle preserved.

## 5. Validate

- [ ] 5.1 Run `just ci` (includes `openspec-validate`).
