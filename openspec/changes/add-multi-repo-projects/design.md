# Design

## The core-model question: what IS a multi-repo project?

Three shapes were considered:

1. **Workspace holds N repos** (VS Code multi-root, what orca #1099 sketches).
   Rejected. `workspace = repo` is load-bearing across the codebase: every
   cache table (`pr_cache`, `ci_runs_cache`, `diff_cache`, `commit_cache`,
   `loc_cache`, …) keys on worktree/repo; the merge queue, PR queue, repo
   trust, sandbox prep, env compose, and the sidebar tree all assume it.
   Multi-root would be a rewrite of the substrate to gain a convenience the
   grouping layer delivers additively.
2. **Reuse zones.** Zones already group workspaces (`workspaces.zone_id`) —
   but a zone is a _soft security firewall_ (credential sub-vault, egress
   intersection, budget rollup). Overloading it with workflow grouping means
   "group these repos for a feature" silently becomes "re-scope these repos'
   credentials", which a security mechanism must never let happen by
   side-effect. Also, one client zone can legitimately contain several
   products/projects. Rejected; the _shape_ is reused, not the table.
3. **A `projects` grouping layer above workspaces** — chosen. Same proven
   membership mechanics as zones: `projects` table (unique name) + nullable
   exclusive `workspaces.project_id`, explicit assignment, never
   path-inferred, per-profile for free (profiles reroot `XDG_STATE_HOME`).
   Zero policy attached. A workspace may be in one zone and one project
   simultaneously; the axes are orthogonal (a project MAY span zones — the
   grouping is visual/workflow-only and mixes no credentials; a config-issue
   style warning for that case is a possible follow-up, see Open questions).

Naming: the codebase already uses "project" for two other things — tracker
projects (`[issues.*] project_key` / `project_id`, provider-side data) and
the historical "project-level in-repo config" roadmap phrasing (roadmap 186,
meaning per-repo config). Neither is a shipped CLI noun. THE-33's user
language is "a single project", so the thegn noun is `project`; docs and help
disambiguate ("tracker projects live under `[issues]`; `thegn project`
groups repos").

## Data model and migration

- `projects (project_id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE,
created_at INTEGER NOT NULL, position INTEGER NOT NULL DEFAULT 0)` —
  `position` for manual sidebar ordering of project headers, same
  exact-order persistence style as `set_workspace_order`.
- `ALTER TABLE workspaces ADD COLUMN project_id INTEGER` (nullable; NULL =
  unprojected). One column, not a join table: membership exclusive by
  construction, mirroring zones.
- **Additive migration, next `user_version` bump** on the ladder
  (`db_migrate.rs`), `CREATE TABLE IF NOT EXISTS` + idempotent `ALTER` so
  parallel-branch DBs tolerate it; do not pin the version number in code
  review — take the next free slot at land time (known collision class).
- Store: `ProjectStore` (`db_projects.rs`) — `create/rename/list (with
member counts)/delete(force)` (delete refuses non-empty unless forced,
  which unassigns members first), `assign`, `project_of_workspace`,
  `set_project_order`. All pure-SQL, unit + migration tested (the zones
  test suite is the template).

## Feature sets: linked naming, derived not persisted

The cross-repo unit of work is a **feature set**: the group of worktrees, one
per member repo, whose branches share the same name.

- **Identity = exact branch-name equality.** `thegn wt new payments-retry
--project shop` resolves ONE final branch name once — the configured
  `branch_prefix` + slug (e.g. `tg/payments-retry`) — and creates that exact
  branch verbatim in every member repo. Per-repo `branch_prefix` overrides
  are deliberately NOT re-applied per member: identity must be literal.
  _Alternative considered_: tail-equality after stripping each repo's own
  prefix — rejected as ambiguous (which prefix produced `tg/x` vs `feat/x`?)
  and fragile under prefix reconfiguration.
- **Derivation is pure core logic.** `thegn_core::project::feature_sets(
members: &[WorkspaceRef], worktrees: &[WorktreeRef]) -> Vec<FeatureSet>`
  groups by branch name over data the model already holds; deterministic
  order (branch name, then member repo order). No new git calls, no
  persisted link rows — git stays the sole source of truth per repo, and a
  branch created outside thegn (plain `git worktree add` in a member) joins
  its feature set automatically on the next hydration. This satisfies (e):
  no super-repo state invented.
- **Partial failure**: batched creation runs the existing per-repo pipeline
  (`thegn_core::worktree` name → base → `git worktree add` → DB register)
  member by member, each independent; outcomes are reported per member and
  a failure never rolls back siblings. Re-running the same create **attaches**
  — members that already have the branch are reported `exists` and skipped —
  so retry-after-partial-failure is the designed recovery path.
- **Sparse sets are normal.** Not every feature touches every member; a
  feature set is whichever members have the branch. `--repos a,b` creates in
  a subset up front.

## Queue semantics (c): honest, per-repo, batched enqueue only

Atomic cross-repo landing is impossible: two object databases (or two
forges' protected branches) share no transaction, and pretending otherwise
would fabricate a guarantee the merge queue's whole design (fold + gate +
CAS per repo) exists to keep honest. Semantics by phase:

- **This change**: `thegn merge add --project <p> --feature <branch>`
  resolves the feature set and enqueues each member's branch in _that
  repo's_ per-repo queue as an ordinary independent row. Draining is
  unchanged — per repo, serial, oldest first. The only cross-repo artifact
  is the batch of enqueues plus per-member reporting.
- **Follow-up (not this change)**: ordered draining — a configured
  `[project.<name>] land_order = ["shared-lib", "api", "web"]` walked with
  stop-on-failure (a deferred/`needs_human` member halts later members'
  drains and reports the halt). Same for a project-grouped PR-queue view
  over `add-pr-queue`'s per-repo rows. Both are additive over this change's
  substrate and are listed as non-goals in the proposal.

## Aggregation (d)

`cross-worktree-aggregation` today aggregates across every worktree of one
workspace. Delta: the pure aggregation model accepts a **project scope** —
excerpts collected from the feature set's worktrees across member repos —
with labels repo-qualified (`repo · worktree`) so rows from different repos
stay identifiable, ordering still deterministic. Population follows the
existing rule: off the event loop, from the same caches (CI cache, dirty
files, content matches), delivered over a channel + `TerminalWaker` pulse.
No new blocking work on the loop.

## Sidebar and rendering

- Member workspaces render under a collapsible **project header row**
  (name + member count + rolled-up attention, same tier-granular bubbling
  rules workspaces already use); unprojected workspaces render exactly as
  today, after the projected groups. Rail mode keeps header identity.
- Ordering: projects order by `projects.position` (keyboard reorder +
  drag, exact-order persistence); member workspaces keep their existing
  positions within the group. This sits directly on the run-partition
  machinery `add-sidebar-folder-ordering` introduces and the row-model
  cleanups in `stabilize-sidebar-internals` — this change builds on their
  post-change shape (dependency, called out in tasks).
- Collapse state persists via `ui_state` keys and follows the
  tombstone-free rule (delete-on-clear, prefix-prune on project delete).
- Glyphs route through the capability glyph table (`caps::active_glyphs()`);
  no literals at draw sites (ratchet).
- **Render damage channel: `Full`** — header rows are chrome; batched
  create/status updates arrive as model changes that mark chrome dirty.
  Pane output is untouched (`Panes` path unaffected); idle stays 0% (no new
  polling — hydration events come from existing off-thread producers).

## CLI and capability catalog

- `thegn project list|create|rename|rm [--force]|assign <project|none>
[repo]` — mirrors `thegn zone`'s grammar and reuses its
  resolve-repo-from-cwd behavior; `--json` on list via the one emitter.
- `thegn wt new <name> --project <p> [--repos …]` extends the existing
  headless creation; `thegn merge add --project <p> --feature <branch>`
  extends the merge namespace.
- Every new verb/parameterization is a `thegn_core::capability::CATALOG`
  row projected across CLI/control/MCP surfaces and gated by
  `required_scope(verb)`: reads (`project list`) under the read scope,
  writes (`create/rename/rm/assign`, batched `wt new`, batched
  `merge add`) under the corresponding write scopes. No second policy
  table. Note: a parallel branch is implementing write MCP tools with
  scope gating — surfacing these verbs over MCP depends on that landing;
  the CATALOG rows themselves do not.

## Help

New action ids (project header collapse/reorder, project-scoped create,
project assign via palette if added) are claimed by a new
`docs/help/projects.md` (help ratchet + prose ratchet). Sidebar header
interactions document under the existing `zone:sidebar` context; the
generated keybindings page picks up new binds automatically.

## Security

- **Projects are not a security boundary.** Membership carries zero policy:
  assigning/unassigning a project never changes credential scope, egress,
  budget, sandbox profile, or env-bundle visibility — those remain zones'
  and the sandbox engine's exclusive job. The design keeps this true
  structurally: nothing in the launch/env/proxy resolve paths reads
  `project_id`.
- **Blast radius of batched creation**: one action now writes to N repos
  (branch + worktree + DB row each). Each member runs the full existing
  per-repo pipeline unchanged — repo trust, sandbox prep (lazy, on first
  open), env resolution — so no per-repo protection is bypassed by the
  batch. The CLI reports the member list and per-member outcomes; the TUI
  wizard shows members before confirming. No `--force` path skips a member
  repo's own confirmations.
- **Batched enqueue** inherits the merge queue's guarantees per repo
  verbatim (object-DB fold + gate + CAS; the agent never lands). The batch
  adds no new landing path.
- **Credentials**: none touched; no new network surface; no tokens in
  config. New DB rows are names + membership only.
- **Catalog scopes**: all new write verbs are write-scoped CATALOG rows, so
  remote surfaces (control API/MCP) cannot reach them without the
  corresponding grant.

## Testability

- Pure core (95% line gate): `feature_sets` derivation, batched-create
  name resolution + plan (which members, exists/skip classification),
  `ProjectStore` SQL, project-scoped aggregation model, sidebar row
  partition with project headers. All table-testable without I/O.
- Seam/I/O: batched `git worktree add` across fixture repos in
  `test/smoke.sh` (with `-c commit.gpgsign=false` per the fixture
  convention, isolated `XDG_STATE_HOME`); sidebar rendering via the muse
  e2e suite only if a frame changes (re-record with `just e2e-update`).

## Open questions

1. **Member order for future `land_order`** — config list per project
   (declarative, diffable) vs deriving from sidebar member order
   (zero-config but conflates display with landing order). Leaning config
   (`[project.<name>] land_order`), deferred with the ordered-drain
   follow-up; no config key ships in this change.
2. **Project spanning zones** — allowed (policy is unaffected), but should
   `thegn doctor`/config-issues warn when a project mixes zones, since the
   sidebar will visually interleave two clients' repos under one header?
3. **TUI management surface** — zones shipped CLI-only (palette deferred);
   projects likely want at least "assign to project" in the sidebar
   workspace context menu in phase 1. Scoped as a should-have task, cut to
   CLI-only if the sidebar in-flight changes make it churn.
4. **`thegn project status`** — is a dedicated feature-set list view needed,
   or does the project-scoped Across section cover it? Phase 2 ships the
   Across scope first; a dedicated section only if usage demands it.
