# Design

## TrackerBackend trait + TrackerCaps (capability-gated provider)

Generalize `crates/superzej-svc/src/issue/mod.rs` `trait IssueBackend` into
`trait TrackerBackend: Send + Sync`, mirroring `crates/superzej-svc/src/ci.rs`
`trait CiProvider` + `fn caps(&self) -> CiCaps`:

```rust
// crates/superzej-svc/src/issue/mod.rs
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
    async fn list_cycles(&self, project_id: &str) -> Result<Vec<Cycle>>; // caps.cycles
    async fn available_transitions(&self, id: &str) -> Result<Vec<Transition>>; // caps.transitions
    async fn transition(&self, id: &str, to: &str) -> Result<WorkItem>;
    async fn add_comment(&self, id: &str, body: &str) -> Result<IssueComment>; // caps.comments
}
```

Static-dispatch enum mirrors `ci::CiClient`:

```rust
pub enum TrackerClient { Linear(..), Github(..), GithubProjects(..), Jira(..), Gitlab(..) }
```

`IssueBackend` is preserved as a thin default-impl shim over `TrackerBackend`
during migration so existing callers (`Section::Issues` loader) compile
unchanged. Methods behind an unset cap return a typed `Err(Unsupported)` rather
than panicking, exactly like CI providers degrade.

## WorkItem supersedes Issue (datamodel)

`crates/superzej-core/src/issue.rs`: introduce `WorkItem` carrying every existing
`Issue` field (`id: "provider:key"`, `number`, `provider`, `title`, `body`,
`priority`, `assignees`, `labels`, `url`, `branch_hint`, `updated_at_ms`,
`project_ids`, `blocked_by`) plus:

```rust
pub enum WorkKind { Issue, Task, Bug, Story, Epic, Subtask } // config_enum!
pub enum WorkStatus { Backlog, Todo, InProgress, Done, Cancelled } // generalizes IssueStatus
pub struct WorkItem {
    /* ...existing Issue fields... */
    pub kind: WorkKind,
    pub parent_id: Option<String>,         // subtask/epic hierarchy
    pub cycle_id: Option<String>,
    pub estimate: Option<f64>,
    pub custom_fields: BTreeMap<String, String>,
    pub status: WorkStatus,
    pub status_raw: String,                // provider's literal workflow state, like CiState
}
pub struct Project { pub id: String, pub name: String, pub key: String,
    pub state: String, pub lead: Option<String>, pub target_date: Option<String>,
    pub progress: f32, pub url: String }
pub struct Cycle { pub id: String, pub name: String, pub project_id: String,
    pub starts_at: Option<i64>, pub ends_at: Option<i64> }
pub struct Transition { pub id: String, pub name: String, pub to_status: WorkStatus }
```

`status_raw` preserves Jira/Linear workflow-state names (e.g. "In Review",
"Ready for QA") that do not map onto the five canonical buckets — the same
raw-name-preservation contract `ci::status_raw` upholds. `IssueStatus` maps onto
`WorkStatus` 1:1 so the existing enum becomes a `From` alias.

## Relation kinds (reuse issue_relations.kind)

`crates/superzej-core/src/issue.rs` relation enum extends beyond `blocked_by`:
`RelationKind { Blocks, BlockedBy, ParentOf, ChildOf, Relates, Duplicates }`,
persisted over the existing `issue_relations (from_id, to_id, kind)` rows — no
schema change for relations, only new `kind` string values.

## Seam / wiring

- **Config** (`crates/superzej-core/src/config.rs`): extend `config_enum!`
  `IssueProviderKind { Linear, Github, Jira, None }` with `GithubProjects` and
  `Gitlab`; each gets an `[issues.<provider>]` sub-table in `IssuesConfig`
  (`linear`, `github_issues`, `jira`, plus new `github_projects`, `gitlab`),
  with secrets via the existing `expand_env_ref("env:VAR")` token path.
  `WorkKind`/`WorkStatus` use the `config_enum!` macro for stable serde.
- **Router** (`crates/superzej-svc/src/issue/mod.rs`): `RouterInner` gains
  `GithubProjects` and `Gitlab` arms; `IssueRouter::list_per_provider` (used for
  cache diffing) and fan-out are reused verbatim. `id: "provider:key"` routing is
  generalized so the router dispatches `get_item`/`transition`/`add_comment` to
  the owning provider by splitting on the `provider:` prefix.
- **Hydration** (`crates/superzej-host/src/hydrate.rs`): reuse
  `RefreshKind::Issues` + `spawn_issue_cache_refresh()`; the off-thread refresh
  also warms `project_cache`. A new `RefreshKind` variant is NOT required —
  projects/cycles ride the existing Issues refresh on `spawn_blocking`, ending
  with a single `TerminalWaker` pulse.
- **Panel** (`crates/superzej-host/src/panel/mod.rs`): `Section::Issues` renders
  the multi-tier tree (Project → Cycle → WorkItem → subtask) and the kanban board
  when `caps().boards`; `Section::Mine` is unchanged. Transition/comment/assignee
  write-back actions are gated on the active provider's `caps()`.
- **Worktree binding** (`issue_links`): generalize worktree-from-item and
  status-on-merge across providers; a `[issues] auto_in_progress` toggle fires
  `transition(id, "In Progress")` on worktree-create when `caps().transitions`.
  `worktree set --comment <text>` writes through `add_comment` to the linked item.

## Rendering & event loop

Damage channel: **chrome** only. Tracker hydration runs off-loop on
`spawn_blocking`, writes `issue_cache`/`project_cache`, then sends on the tokio
mpsc channel and pulses `TerminalWaker`; the loop drains, marks the master
`dirty` (panel content changed), and `render_plan::plan()` yields `Full` (chrome
recompose) — never a per-pane `Panes` decision. An idle wake with no tracker
delta stays `Skip`. No tick, no timeout, no blocking I/O on the loop. The
existing `RefreshKind::Issues` cadence test in `hydrate.rs` continues to hold.

## Persistence

SQLite (`crates/superzej-core/src/db.rs`): add a `project_cache (repo_root,
provider, json, fetched_at)` table mirroring `issue_cache`, for the Project/Cycle
tier. `WorkItem`/`Cycle` are serialized into the existing `issue_cache.json`
column (JSON is forward-compatible — new fields are additive, old rows
deserialize with serde defaults). `issue_relations.kind` gains new string values
(no DDL). **Bump `SCHEMA_VERSION` 21 → 22** and add the `project_cache` migration
branch; `user_version` is set on upgrade as today.

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
