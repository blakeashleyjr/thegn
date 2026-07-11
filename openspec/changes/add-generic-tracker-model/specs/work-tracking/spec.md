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
Cycle/sprint tier (`Cycle { id, name, project_id, starts_at, ends_at }` via
`list_cycles`), and these tiers MUST reuse `WorkItem.project_ids` and
`WorkItem.cycle_id` to associate items rather than introducing a parallel index.

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

### Requirement: Multi-provider router with provider:key routing

Linear, Jira, GitHub Issues, GitHub Projects v2, and GitLab SHALL each be a
`config_enum!` `IssueProviderKind` value with an `[issues.<provider>]` sub-table
(secrets via `env:` tokens), registered behind `IssueRouter`, and the router MUST
route per-item operations to the owning provider by the `provider:` prefix of the
`id` while reusing `list_per_provider` for cache diffing.

#### Scenario: Operation routes to the owning provider

- **WHEN** `transition("gitlab:42", to)` is invoked on a router holding both Linear and GitLab providers
- **THEN** the router dispatches only to the GitLab provider and leaves the Linear provider untouched

### Requirement: Worktree binding with auto-transition and checkpoint sync

Worktree-from-item creation and status-on-merge SHALL work uniformly across all
providers via `issue_links`, an `auto_in_progress` toggle MUST move a linked item
to "In Progress" on worktree-create when `caps().transitions`, and `worktree set
--comment <text>` MUST write through to the linked item as a comment when
`caps().comments`.

#### Scenario: Worktree creation transitions the item

- **WHEN** `auto_in_progress` is enabled and a worktree is created from a linked Linear item in `Todo`
- **THEN** the item transitions to `InProgress` via `transition()` and the link is recorded in `issue_links`

### Requirement: Two-way write-back and kanban board are AI-free

Comment, status-transition, and assignee write-back plus the kanban board view SHALL function with no AI or proxy layers present (gated only on provider `caps()`), and the work-tracking capability MUST NOT import or hard-depend on any AI/agent crate — `AgentDispatch` linkage remains optional and additive.

#### Scenario: Tracking works with AI layers absent

- **WHEN** thegn runs as the AI-free shell with the proxy/agent layers disabled
- **THEN** listing, filtering, transitions, comments, and the board all operate normally and no AI crate is required to build or run the tracker
