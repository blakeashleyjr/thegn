# Work Tracking

## ADDED Requirements

### Requirement: Capability-gated tracker provider trait

Every tracker provider SHALL implement a `TrackerBackend` trait that declares its
feature set through `fn caps() -> TrackerCaps { projects, cycles, subtasks,
transitions, custom_fields, comments, boards, create }`, mirroring
`CiProvider::caps()`, and the router and chrome MUST consult those capabilities so
a thin provider degrades — hiding or refusing unsupported operations — rather than
fabricating features it cannot back.

#### Scenario: Thin provider hides unsupported tier

- **WHEN** a provider reports `caps().projects == false`
- **THEN** `list_projects` returns a typed `Unsupported` error and the panel omits the project tier instead of synthesizing an empty or fake project

### Requirement: WorkItem supersedes Issue with raw status preservation

The work model SHALL be a `WorkItem` that carries every existing `Issue` field
plus `kind`, `parent_id`, `cycle_id`, `estimate`, `custom_fields`, a canonical
`status: WorkStatus`, and a `status_raw` string, and `status_raw` MUST preserve
the provider's literal workflow-state name even when it does not map onto a
canonical `WorkStatus` bucket.

#### Scenario: Non-canonical Jira state is preserved

- **WHEN** a Jira item is in workflow state "Ready for QA"
- **THEN** the `WorkItem` canonicalizes `status` to `InProgress` while `status_raw` retains "Ready for QA" for display and transition lookup

### Requirement: Project and Cycle tiers

The model SHALL expose a Project/Epic tier (`Project { id, name, key, state,
lead, target_date, progress, url }` via `list_projects`/`project_items`) and a
Cycle/sprint tier (`Cycle { id, name, scope_id, starts_at, ends_at }` via
`list_cycles(scope_id)`, where the scope is the owning **team** — Linear cycles
belong to a team, not a project), and these tiers MUST reuse
`WorkItem.project_ids` and `WorkItem.cycle_id` to associate items rather than
introducing a parallel index.

#### Scenario: Item resolves into its project and cycle

- **WHEN** a `WorkItem` has `project_ids = ["linear:PRJ-1"]` and `cycle_id = Some("linear:CYC-7")`
- **THEN** it appears under that project and cycle in the tier rendering, sourced from the same identifiers without a separate membership table

### Requirement: Status transitions instead of free-set

When a provider declares `caps().transitions`, status changes SHALL go through
`available_transitions()` then `transition(id, to)` rather than an arbitrary
set-status write, so workflow-constrained providers (Jira) only ever apply legal
transitions, and providers without the capability MUST fall back to a simple
status update.

#### Scenario: Only legal transitions are offered

- **WHEN** the user opens the status menu for a Jira item whose workflow allows only "Start Progress" and "Cancel"
- **THEN** the menu lists exactly those transitions from `available_transitions()` and applying one calls `transition()` with the chosen id

### Requirement: Relation kinds beyond blocking

Item relations SHALL support `Blocks`, `BlockedBy`, `ParentOf`, `ChildOf`,
`Relates`, and `Duplicates`, persisted over the existing `issue_relations
(from_id, to_id, kind)` rows, and the parent/child kinds MUST drive the
subtask/epic hierarchy via `WorkItem.parent_id` without a schema change to the
relations table.

#### Scenario: Subtask links to its parent

- **WHEN** a subtask declares `parent_id = "linear:ENG-10"`
- **THEN** a `ChildOf` relation row is recorded and the subtask renders nested under ENG-10 in the tree

### Requirement: Multi-provider router with instance:key routing

Linear, Jira, and GitHub Issues SHALL each be a `config_enum!`
`IssueProviderKind` value with an `[issues.<provider>]` sub-table (secrets via
`env:`/`file:` tokens), registered behind `IssueRouter`. Linear SHALL be the
reference provider exercising every capability (all caps true except
`custom_fields`); Jira and GitHub Issues keep their current methods and MUST
advertise honest, mostly-false caps, returning `IssueError::Unsupported` for
the rest (GitHub Projects v2 and GitLab providers are future work). The router
MUST route per-item operations to the owning provider instance by the
`<instance>:` prefix of the `id` via `split_once(':')` while reusing
`list_per_provider` for cache diffing, and `backend_for_id` MUST fall back from
the exact instance to the first provider of that kind for legacy ids.

#### Scenario: Operation routes to the owning provider

- **WHEN** `transition("jira:PROJ-42", to)` is invoked on a router holding both Linear and Jira providers
- **THEN** the router dispatches only to the Jira provider and leaves the Linear provider untouched

#### Scenario: Legacy id falls back to the first provider of its kind

- **WHEN** `get_item("linear:ENG-123")` is invoked and only named instances `linear@work`/`linear@oss` are configured
- **THEN** `backend_for_id` routes the legacy `linear` id to the first configured Linear instance instead of failing

### Requirement: Multi-account tracker instances

thegn SHALL support named Linear accounts under
`[issues.linear.accounts.<name>]` — each with an `api_key` secrets-ref
(`env:`/`file:`, or `""` meaning the stored login) and default `team`/`project`
— exposed as router instances `"linear"` (legacy single account) and
`"linear@<account>"`, with item ids of the form `"<instance>:<key>"` (e.g.
`linear@work:ENG-123`) so `@` keeps `split_once(':')` routing context-free. A
workspace SHALL bind to an instance and scope via `[workspace.<slug>] tracker =
{ provider, account, team, project }`, and the per-repo `.thegn.toml`
`[issues.linear]` overlay SHALL accept `account`/`team`/`project` (`team_id`
kept as a deprecated alias). The effective scope MUST resolve with precedence
repo overlay → workspace binding → account defaults → legacy `team_id`, via the
pure `Config::repo_issues_scoped(root, slug)`.

`thegn tracker login linear [--account NAME]` SHALL interactively validate and
store an API key at `$XDG_STATE_HOME/thegn/accounts/linear/<name>/api_key`
(mode 0600); key resolution MUST prefer a resolving config ref, then the stored
login, and otherwise skip the instance with a warning rather than failing the
router.

#### Scenario: Item id routes to its account instance

- **WHEN** `add_comment("linear@work:ENG-123", body)` is invoked with accounts `work` and `oss` configured
- **THEN** the router dispatches to the `linear@work` instance using that account's API key and team/project defaults

#### Scenario: Repo overlay wins scope resolution

- **WHEN** a repo's `.thegn.toml` sets `[issues.linear] account = "oss"` while the workspace binding names `account = "work"`
- **THEN** `Config::repo_issues_scoped` resolves the `oss` account for that repo, falling back to the workspace binding only where the overlay is silent

#### Scenario: Missing key skips the instance

- **WHEN** an account's `api_key` ref does not resolve and no stored login exists
- **THEN** that instance is skipped with a warning and the remaining instances keep working

### Requirement: HouseTracker MCP tools with gated writes

thegn SHALL expose the tracker to agents as a `HouseTracker` MCP tool family
following the house-tool pattern: read tools `tracker_my_issue`,
`tracker_list`, `tracker_get`, and `tracker_search` MUST serve the DB cache
with a staleness line (fetching live only when `refresh=true`); write tools
`tracker_update_status`, `tracker_comment`, and `tracker_assign` MUST be gated
by `[issues] agent_write` (global default `false`) with a
`[workspace.<slug>] issues_agent_write` override, and when disabled MUST be
both omitted from `tools/list` and refused at dispatch. An omitted `id` on a
write tool SHALL resolve to the worktree's linked issue via `issue_links`.
Writes MUST run live and then synchronously refresh the affected cache rows.

#### Scenario: Writes are absent and refused when disabled

- **WHEN** `agent_write` is false (the default) and an agent lists tools then attempts `tracker_comment`
- **THEN** the write tools do not appear in `tools/list` and the dispatch is refused with an error

#### Scenario: Omitted id targets the linked issue

- **WHEN** `tracker_update_status` is called without `id` from a worktree linked to `linear@work:ENG-123` and writes are enabled for the workspace
- **THEN** the status write applies to ENG-123 live and the affected cache rows are refreshed synchronously

### Requirement: Worktree binding with auto-transition and checkpoint sync

Worktree-from-item creation and status-on-merge SHALL work uniformly across all
providers via `issue_links`, an `[issues] auto_in_progress` toggle MUST move a
linked item to "In Progress" on worktree-create when `caps().transitions`,
`[issues] move_on_pr_open` MUST move a linked item to the configured
`in_review_state` when its PR opens, `move_on_merge` MUST also cover local
`thegn land` merges (via the fold-actor drain), and `worktree set --comment
<text>` MUST write through to the linked item as a comment when
`caps().comments`.

#### Scenario: Worktree creation transitions the item

- **WHEN** `auto_in_progress` is enabled and a worktree is created from a linked Linear item in `Todo`
- **THEN** the item transitions to `InProgress` via `transition()` and the link is recorded in `issue_links`

#### Scenario: A local land counts as a merge

- **WHEN** `move_on_merge` is enabled and a linked worktree's branch lands on main via `thegn land` (no forge PR merge)
- **THEN** the linked item receives the same on-merge transition as a forge merge would trigger

### Requirement: Two-way write-back and the grouped status view are AI-free

Comment, status-transition, and assignee write-back plus the grouped ByStatus
view SHALL function with no AI or proxy layers present (gated only on provider
`caps()`), and the work-tracking capability MUST NOT import or hard-depend on
any AI/agent crate — `AgentDispatch` linkage remains optional and additive, and
the `HouseTracker` MCP tools are strictly additive on top of this AI-free
substrate. (A kanban board view is deferred to the follow-up change
`add-tracker-board-view`.)

#### Scenario: Tracking works with AI layers absent

- **WHEN** thegn runs as the AI-free shell with the proxy/agent layers disabled
- **THEN** listing, filtering, transitions, comments, and the grouped ByStatus view all operate normally and no AI crate is required to build or run the tracker
