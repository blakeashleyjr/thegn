# Design

## TrackerBackend trait + TrackerCaps (capability-gated provider)

Generalize `crates/thegn-svc/src/issue/mod.rs` `trait IssueBackend` into
`trait TrackerBackend: Send + Sync`, mirroring `crates/thegn-svc/src/ci.rs`
`trait CiProvider` + `fn caps(&self) -> CiCaps`:

```rust
// crates/thegn-svc/src/issue/mod.rs
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TrackerCaps {
    pub projects: bool,
    pub cycles: bool,
    pub subtasks: bool,
    pub transitions: bool,
    pub custom_fields: bool,
    pub comments: bool,
    pub boards: bool,
    pub create: bool,
}

#[async_trait]
pub trait TrackerBackend: Send + Sync {
    fn provider_id(&self) -> &str;
    fn caps(&self) -> TrackerCaps;
    async fn list_items(&self, f: &WorkFilter) -> Result<Vec<WorkItem>>;
    async fn get_item(&self, id: &str) -> Result<WorkItem>;
    async fn create_item(&self, d: &WorkDraft) -> Result<WorkItem>;     // caps.create
    async fn update_item(&self, id: &str, p: &WorkPatch) -> Result<WorkItem>;
    async fn search(&self, q: &str) -> Result<Vec<WorkItem>>;
    async fn list_projects(&self) -> Result<Vec<Project>>;              // caps.projects
    async fn project_items(&self, project_id: &str) -> Result<Vec<WorkItem>>;
    async fn list_cycles(&self, scope_id: &str) -> Result<Vec<Cycle>>;  // caps.cycles (team scope — Linear cycles belong to a team, not a project)
    async fn available_transitions(&self, id: &str) -> Result<Vec<Transition>>; // caps.transitions
    async fn transition(&self, id: &str, to: &str) -> Result<WorkItem>;
    async fn add_comment(&self, id: &str, body: &str) -> Result<IssueComment>; // caps.comments
}
```

Static-dispatch enum mirrors `ci::CiClient`:

```rust
pub enum TrackerClient { Linear(..), Github(..), Jira(..) }
```

(GitHub Projects v2 and GitLab arms are future work — see Non-goals.)

The `IssueBackend` shim collapses to a re-export — `pub use TrackerBackend as
IssueBackend` — so existing callers (`Section::Issues` loader) compile
unchanged. Methods behind an unset cap return a typed
`Err(IssueError::Unsupported)` rather than panicking, exactly like CI providers
degrade.

**Provider scope for this change: Linear is the reference provider** and
exercises every capability (all caps true except `custom_fields`). Jira and
GitHub Issues keep their current methods and advertise honest, mostly-false
caps, inheriting the `IssueError::Unsupported` default impls for the new
methods; completing Jira and adding GitHub Projects v2 / GitLab are follow-up
changes.

## WorkItem supersedes Issue (datamodel)

`crates/thegn-core/src/issue.rs`: `WorkItem` is implemented as an **in-place
extension of the existing `Issue` struct** — `pub use Issue as WorkItem` and
`pub use IssueStatus as WorkStatus` — keeping every existing field
(`id: "provider:key"`, `number`, `provider`, `title`, `body`, `priority`,
`assignees`, `labels`, `url`, `branch_hint`, `updated_at_ms`, `project_ids`,
`blocked_by`) and adding the new fields directly, all **serde-defaulted** so
old `issue_cache` JSON rows deserialize forward-compatibly:

```rust
pub enum WorkItemKind { Issue, Task, Bug, Story, Epic, Subtask } // config_enum!
// (named WorkItemKind — `WorkKind` collides with the existing thegn_core::work::WorkKind)
pub use IssueStatus as WorkStatus; // Backlog, Todo, InProgress, Done, Cancelled
pub use Issue as WorkItem;
pub struct Issue {
    /* ...existing Issue fields... */
    #[serde(default)] pub kind: WorkItemKind,
    #[serde(default)] pub parent_id: Option<String>,   // subtask/epic hierarchy
    #[serde(default)] pub cycle_id: Option<String>,
    #[serde(default)] pub estimate: Option<f64>,       // drops derive(Eq); keep PartialEq
    #[serde(default)] pub custom_fields: BTreeMap<String, String>,
    pub status: WorkStatus,
    #[serde(default)] pub status_raw: String,          // provider's literal workflow state, like CiState
}
pub struct Project { pub id: String, pub name: String, pub key: String,
    pub state: String, pub lead: Option<String>, pub target_date: Option<String>,
    pub progress: f32, pub url: String }
pub struct Cycle { pub id: String, pub name: String, pub scope_id: String, // team scope, not project
    pub starts_at: Option<i64>, pub ends_at: Option<i64> }
pub struct Transition { pub id: String, pub name: String, pub to_status: WorkStatus }
```

`status_raw` preserves Jira/Linear workflow-state names (e.g. "In Review",
"Ready for QA") that do not map onto the five canonical buckets — the same
raw-name-preservation contract `ci::status_raw` upholds. Because `WorkStatus`
_is_ `IssueStatus`, no `From` mapping is needed; the `estimate: Option<f64>`
field means `Issue` drops its `Eq` derive (keeping `PartialEq`).

## Relation kinds (reuse issue_relations.kind)

`crates/thegn-core/src/issue.rs` relation enum extends beyond `blocked_by`:
`RelationKind { Blocks, BlockedBy, ParentOf, ChildOf, Relates, Duplicates }`,
persisted over the existing `issue_relations (from_id, to_id, kind)` rows — no
schema change for relations, only new `kind` string values.

## Seam / wiring

- **Config** (`crates/thegn-core/src/config.rs`): the existing `config_enum!`
  `IssueProviderKind { Linear, Github, Jira, None }` is unchanged in this change
  (`GithubProjects`/`Gitlab` variants arrive with their follow-up providers);
  the existing `[issues.<provider>]` sub-tables (`linear`, `github_issues`,
  `jira`) keep secrets via the existing `expand_env_ref("env:VAR")` token path.
  New `[issues]` keys: `agent_write` (default false), `auto_in_progress`,
  `move_on_pr_open`, `in_review_state`, `meta_ttl_secs`; the existing
  `move_on_merge` is extended to cover local `thegn land` merges via the
  fold-actor drain. `WorkItemKind`/`WorkStatus` use the `config_enum!` macro
  for stable serde.
- **Router** (`crates/thegn-svc/src/issue/mod.rs`): `RouterInner` arms are
  unchanged (Linear/Github/Jira); `IssueRouter::list_per_provider` (used for
  cache diffing) and fan-out are reused verbatim. `id: "<instance>:<key>"`
  routing is generalized so the router dispatches
  `get_item`/`transition`/`add_comment` to the owning provider instance by
  `split_once(':')` (see Multi-account instances below).
- **Hydration** (`crates/thegn-host/src/hydrate_tracker.rs`, new module beside
  `hydrate.rs`): reuse `RefreshKind::Issues` + `spawn_issue_cache_refresh()`;
  the off-thread refresh also warms the activated `issue_projects` table and
  `tracker_meta`. A new `RefreshKind` variant is NOT required — projects/cycles/
  metadata ride the existing Issues refresh on `spawn_blocking`, ending with a
  single `TerminalWaker` pulse.
- **Panel** (`crates/thegn-host/src/panel/sections/issues/{mod,list,detail,tiers,md}.rs`):
  `Section::Issues` renders the multi-tier tree (Project → Cycle → WorkItem →
  subtask) and a **grouped ByStatus view** (the kanban board is deferred to the
  follow-up change `add-tracker-board-view`); `Section::Mine` is unchanged.
  Transition/comment/assignee write-back actions are gated on the active
  provider's `caps()`.
- **Worktree binding** (`issue_links`): generalize worktree-from-item and
  status-on-merge across providers; a `[issues] auto_in_progress` toggle fires
  `transition(id, "In Progress")` on worktree-create when `caps().transitions`;
  `[issues] move_on_pr_open` moves a linked item to `in_review_state` when its
  PR opens. `worktree set --comment <text>` writes through `add_comment` to the
  linked item. Pure transition planning lives in
  `crates/thegn-core/src/issue_flow.rs` (unit-tested, 95% gate).

## Multi-account instances

Named Linear accounts live under `[issues.linear.accounts.<name>]` with
`api_key` (an `env:`/`file:` secrets-ref, or `""` ⇒ use the stored login) and
default `team`/`project`.

- **Instance ids**: `"linear"` is the legacy single account;
  `"linear@<account>"` names an account instance. Item ids are
  `"<instance>:<key>"` (e.g. `linear@work:ENG-123`) — `@` keeps
  `split_once(':')` routing context-free. `backend_for_id` falls back from the
  exact instance to the first-of-kind for legacy ids.
- **Workspace mapping**: `[workspace.<slug>] tracker = { provider, account,
team, project }` binds a workspace to an instance + scope. The per-repo
  `.thegn.toml` `[issues.linear]` overlay gains `account`/`team`/`project`
  (`team_id` kept as a deprecated alias). Precedence: repo overlay → workspace
  binding → account defaults → legacy `team_id`, resolved by the pure
  `Config::repo_issues_scoped(root, slug)` (unit-tested).
- **Login**: interactive `thegn tracker login linear [--account NAME]` validates
  the pasted API key and stores it at
  `$XDG_STATE_HOME/thegn/accounts/linear/<name>/api_key` (mode 0600). Key
  resolution order: config secrets-ref (when it resolves) → stored login → skip
  the instance with a warning. API-key paste now; OAuth later behind the same
  seam.

## HouseTracker MCP tools

A `HouseTracker` trait in `crates/thegn-core/src/mcp/mod.rs`, following the
`HouseGit`/`HouseForge`/`HouseMerge` house-tool pattern (trait in core,
implemented in `thegn-svc` over the router):

- **Read tools** — `tracker_my_issue`, `tracker_list`, `tracker_get`,
  `tracker_search`: serve the DB cache with a staleness line; `refresh=true`
  fetches live.
- **Write tools** — `tracker_update_status`, `tracker_comment`,
  `tracker_assign`: gated by `[issues] agent_write = false` (global default)
  with a `[workspace.<slug>] issues_agent_write` override. When disabled, write
  tools are **omitted from `tools/list` AND refused at dispatch**. An omitted
  `id` on a write tool means the worktree's linked issue via `issue_links`.
  Writes are live, then synchronously refresh the affected cache rows.

## Host module layout (god-file ratchet)

New host code lands in sibling modules, never in the pinned god-files
(`run.rs`/`chrome.rs`/`config.rs`/`hydrate.rs` may only shrink):
`crates/thegn-host/src/handlers/tracker.rs` (key handling + actions),
`crates/thegn-host/src/hydrate_tracker.rs` (refresh plumbing), and
`crates/thegn-host/src/panel/sections/issues/{mod,list,detail,tiers,md}.rs`
(rendering). Pure transition planning goes to
`crates/thegn-core/src/issue_flow.rs`.

## Rendering & event loop

Damage channel: **chrome** only. Tracker hydration runs off-loop on
`spawn_blocking`, writes `issue_cache`/`issue_projects`/`tracker_meta`/
`issue_detail_cache`, then sends on the tokio
mpsc channel and pulses `TerminalWaker`; the loop drains, marks the master
`dirty` (panel content changed), and `render_plan::plan()` yields `Full` (chrome
recompose) — never a per-pane `Panes` decision. An idle wake with no tracker
delta stays `Skip`. No tick, no timeout, no blocking I/O on the loop. The
existing `RefreshKind::Issues` cadence test in `hydrate.rs` continues to hold.

## Persistence

SQLite (`crates/thegn-core/src/db.rs`): **bump `SCHEMA_VERSION` 43 → 44**. The
migration adds two tables:

- `tracker_meta (provider, scope, kind, json, fetched_at,
PRIMARY KEY(provider, scope, kind))` — team-scoped metadata (`teams`,
  `states`, `cycles`, `projects` kinds), TTL ~900s (`[issues] meta_ttl_secs`).
- `issue_detail_cache (issue_id PRIMARY KEY, json, fetched_at)` — single-item
  detail (comments + relations), 60s TTL.

There is **no new `project_cache` table** — the existing dormant
`issue_projects` table (same shape) is activated for the Project/Cycle tier.
The migration also clears `issue_cache`/`issue_projects` rows: they are pure
caches, rebuilt by the next refresh. `WorkItem`/`Cycle` are serialized into the
existing `issue_cache.json` column (JSON is forward-compatible — new fields are
additive and serde-defaulted, so old rows deserialize). `issue_relations.kind`
gains new string values (no DDL). `user_version` is set on upgrade as today.

## Invariants

- **0% idle preserved**: no new ticker/timeout; tracker work is off-loop on
  `spawn_blocking` + single waker pulse; idle wakes still `Skip`.
- **Off-loop**: all git/DB/network tracker I/O stays on background tasks; the loop
  only drains channels and re-renders when dirty.
- **AI-additive, never a hard AI dependency**: `TrackerBackend` and the panel work
  with zero AI layers present; `AgentDispatch` linkage remains optional. The
  AI-free shell does not import or require the AI/proxy crates for tracking.
- **Capability honesty**: providers advertise real `TrackerCaps`; the chrome
  hides/greys unsupported actions instead of faking projects/cycles/transitions —
  mirroring the CI `caps()` contract and its `caps_are_set` test.
