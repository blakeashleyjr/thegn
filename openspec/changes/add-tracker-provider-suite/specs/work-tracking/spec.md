# Work Tracking — provider suite delta

> Layers on the `work-tracking` capability introduced by the in-flight
> `add-generic-tracker-model` change; that change's requirements are assumed
> and not restated here.

## ADDED Requirements

### Requirement: Tracker errors classify through the seam vocabulary

`IssueError` SHALL carry a typed `Unsupported` variant and implement
`thegn_core::seam::SeamError`, classifying every variant into `ErrorClass`
(`Unsupported`, `NotConfigured`, `Auth`; `Network` → `Transient` for
connect/timeout and `Other` otherwise; `Subprocess` → `NotInstalled` when the
binary is absent and `Other` otherwise; `Api`/`Parse` → `Other`). Optional-op
default implementations MUST return the typed `Unsupported` error naming the
operation — never a stringly API error — and MUST return it without performing
any I/O.

#### Scenario: Default op refuses with a typed error and no I/O

- **WHEN** a provider that does not override `add_comment` receives an `add_comment` call
- **THEN** it returns an error whose `class()` is `ErrorClass::Unsupported` naming the operation, immediately and without any network or subprocess activity

#### Scenario: Transient network failures classify as Transient

- **WHEN** a provider call fails with a connect or timeout error
- **THEN** the error's `class()` is `ErrorClass::Transient` and `is_transient()` is true, so the connectivity holder — not the auth path — absorbs it

### Requirement: Caps and ops agree, verified offline

Every tracker provider — native and plugin-bridged alike — SHALL pass a shared
offline conformance check over a single `(cap, op)` table: for every optional
operation, a false capability flag MUST mean the operation returns
`ErrorClass::Unsupported` immediately, and a true flag MUST mean an
implementation exists behind it — an unconfigured backend fails with anything
but `Unsupported` (e.g. `NotConfigured` or `Auth`). The check MUST run without
network access, and the per-provider coverage MUST iterate
`IssueProviderKind::ALL` so a newly added provider is covered mechanically.

#### Scenario: Overclaimed capability fails conformance

- **WHEN** a provider's `caps()` reports `comments: true` while it inherits the default `add_comment`
- **THEN** the conformance check fails, because the op returned `Unsupported` behind a true flag

#### Scenario: Underclaimed capability fails conformance

- **WHEN** a provider implements `attach_label` but reports `labels: false`
- **THEN** the conformance check fails, because the chrome would hide a working action behind a false flag

### Requirement: Labels are a declared capability

`TrackerCaps` SHALL include `labels: bool` gating `attach_label` and
`detach_label`, and the panel/CLI MUST offer label actions only when the owning
provider declares `labels: true` — never by probing an error.

#### Scenario: Label actions follow the declared cap

- **WHEN** the selected item belongs to a provider with `labels: true` and another with `labels: false` is also configured
- **THEN** label attach/detach actions appear for the first item and are absent (not erroring) for items of the second provider

### Requirement: Jira reaches its honest ceiling

The Jira provider SHALL implement transitions (`available_transitions` via
`GET /rest/api/3/issue/{key}/transitions`, `transition` via POST of a listed
transition id), comments (minimal ADF paragraph), labels (`update.labels`
add/remove), projects (`/rest/api/3/project/search`), and subtask mapping
(`issuetype.subtask` + `fields.parent` → `kind`/`parent_id`), declaring
`transitions`, `comments`, `labels`, `projects`, and `subtasks` true. `cycles`
SHALL remain false — Jira sprints require Agile-API board discovery, which is
deferred — rather than being synthesized.

#### Scenario: Jira status change applies only a legal transition

- **WHEN** the user changes a Jira item's status
- **THEN** the menu offers exactly the ids returned by `available_transitions` and the chosen id is POSTed — a free-set status write is never attempted against Jira

### Requirement: GitHub Issues reaches its honest ceiling

The GitHub Issues provider SHALL implement comments (`gh issue comment`) and
labels (`gh issue edit --add-label/--remove-label`), dir-anchored and with the
`gh` invocation confined to the provider's implementation file, declaring
`comments` and `labels` true; `projects`, `cycles`, `subtasks`, and
`transitions` SHALL remain false (GitHub Projects v2 is a distinct future
provider, not a capability of this one).

#### Scenario: Commenting on a GitHub issue

- **WHEN** the user comments on a GitHub-provided item from the panel
- **THEN** the comment is posted via `gh issue comment` anchored to the account's directory, and the action succeeded/failed status surfaces in the panel

### Requirement: The tracker seam has no vendor downcasts

The `IssueBackend`/`TrackerBackend` trait SHALL NOT expose concrete-backend
downcasts (`as_kaneo()` is removed). Kaneo's board/project browsing SHALL ride
the generic tier operations (`list_projects`, `project_items`) with
`boards: true`, and the `thegn kaneo project/board/task` verbs MUST route
through the router's generic operations with unchanged user-visible output.

#### Scenario: Kaneo boards render through the generic tier

- **WHEN** `thegn kaneo board` runs against a configured Kaneo account
- **THEN** the board data is fetched via the router's generic tier operations — no downcast to the concrete backend exists on the seam

### Requirement: Tracker login is provider-generic

`thegn tracker login <provider>` SHALL be the single login verb for providers
with a stored-credential flow; Kaneo's device flow moves behind
`thegn tracker login kaneo`, and `thegn kaneo login` SHALL remain as an alias
projecting the same capability-catalog row. Stored credentials live under
`$XDG_STATE_HOME/thegn/` with mode 0600, and probes MUST report availability
and reason without printing token material.

#### Scenario: Both login spellings converge

- **WHEN** a user runs `thegn kaneo login` or `thegn tracker login kaneo`
- **THEN** both run the same device flow, store the same credential, and are authorized by the same capability-catalog row

### Requirement: Notion is a tracker provider

`notion` SHALL be an implemented `IssueProviderKind` backed by the Notion API
(pinned `Notion-Version`), scoped to one data source per account
(`[issues.notion] data_source_id`, with `database_id` accepted and resolved
once to its data source, cached in `tracker_meta`). Property names
(status/assignee/labels/priority) SHALL be configurable with sensible
defaults; status SHALL canonicalize by the status property's group (To-do →
Todo, In progress → InProgress, Complete → Done) with the option name
preserved in `status_raw`, falling back to a name heuristic for plain select
properties. Caps: `comments`, `labels`, and `create` true; tiers and
transitions false. Assignee-me filtering MUST require the configured
`user_id`; when unset the assignee filter is skipped with a one-time warning
rather than returning a silently empty list.

#### Scenario: Notion status group canonicalizes with raw preserved

- **WHEN** a page's status option "Blocked — waiting" belongs to the "In progress" group
- **THEN** the item's canonical status is `InProgress` and `status_raw` is "Blocked — waiting"

#### Scenario: Missing user_id degrades loudly

- **WHEN** `filter_assignee_me` is requested and `[issues.notion] user_id` is unset
- **THEN** the assignee filter is skipped, a one-time warning explains why, and the unfiltered list is returned

### Requirement: Plane is a tracker provider

`plane` SHALL be an implemented `IssueProviderKind` backed by the Plane REST
API (`X-API-Key`; `base_url` defaulting to the hosted endpoint with
self-hosted overrides; `workspace_slug` required). An empty `project_id` MUST
mean workspace-wide: projects are listed (cached in `tracker_meta` with TTL)
and issues fanned in, bounded by `max_issues`. Workflow-state groups
(`backlog`/`unstarted`/`started`/`completed`/`cancelled`) SHALL map onto the
canonical status with the state name preserved in `status_raw`. Caps:
`projects`, `cycles`, `subtasks`, `comments`, `labels`, `create` true;
`transitions` false (states are free-set — the simple status-update path
applies). The provider MUST honor Plane's rate limit by batching per-project
calls and backing off on 429.

#### Scenario: Plane state group maps onto canonical status

- **WHEN** a Plane work item is in a state belonging to the `started` group named "In Review"
- **THEN** the item's canonical status is `InProgress` and `status_raw` is "In Review"

#### Scenario: Rate limit is honored on workspace fan-in

- **WHEN** a workspace-wide refresh spans many projects and the API returns 429
- **THEN** the refresh backs off and completes within the rate limit instead of hammering the instance, and the fan-in stays bounded by `max_issues`
